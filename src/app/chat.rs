//! Native conversation, chat, and poll controllers.
//!
//! Core and network operations are always scheduled with [`AppContext::call`];
//! this module only touches Slint models from its event-loop completions.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use friendshub_core::error::Error as CoreError;
use serde::Deserialize;
use serde_json::{Value, json};
use slint::{Image, ModelRc, VecModel};

use crate::app::{ActiveSession, AppContext};
use crate::features::{chat as chat_api, conversations as conversations_api, profile_settings};
use crate::{
    ChatMenuItem, ChatMessage, ChatReaction, ChatSearchResult, ConversationRow, FilterTab,
    FolderTab, MainWindow, PollDraftOption, PollOptionView, PollViewModel,
};

const HISTORY_PAGE_SIZE: i64 = 200;
const POLL_PAGE_SIZE: i64 = 500;
const ENVELOPE_TYPE_MESSAGE: i64 = 1;
const ENVELOPE_TYPE_SYNC: i64 = 2;
const ENVELOPE_TYPE_EDIT: i64 = 4;
const ENVELOPE_TYPE_DELETE: i64 = 5;
const ENVELOPE_TYPE_REACTION: i64 = 6;

static CONTROLLER_STATE: OnceLock<Mutex<ControllerState>> = OnceLock::new();
static MESSAGE_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct ControllerState {
    accounts: HashMap<String, AccountState>,
}

#[derive(Default)]
struct AccountState {
    conversations: Vec<conversations_api::Conversation>,
    folders: Vec<conversations_api::ChatFolder>,
    list_request_generation: u64,
    request_generations: HashMap<String, u64>,
    selected_filter: String,
    selected_folder: String,
    search: String,
    chats: HashMap<String, ChatState>,
}

#[derive(Default)]
struct ChatState {
    title: String,
    subtitle: String,
    kind: String,
    peer_account_id: String,
    messages: Vec<MessageEntry>,
    polls: Vec<PollState>,
    poll_composer: PollComposerState,
    pins: Vec<String>,
    selected_pin: usize,
    history_cursor: Option<conversations_api::HistoryCursor>,
    history_has_older: bool,
    history_loading_older: bool,
    poll_cursor: Option<chat_api::PollHistoryRequest>,
    poll_has_older: bool,
    polls_loading: bool,
    polls_loading_older: bool,
    polls_error: String,
    poll_selections: HashMap<String, Vec<String>>,
    poll_proposals: HashMap<String, String>,
    loading: bool,
    busy: bool,
    editing: bool,
    error: String,
    composer_error: String,
    draft: String,
    banner_text: String,
    search_open: bool,
    search_query: String,
    search_results: Vec<ChatSearchResultEntry>,
    search_selected_index: i32,
    request_generations: HashMap<String, u64>,
    editing_message_id: Option<String>,
    message_menu_open: bool,
    message_menu_index: i32,
    delete_confirmation_message_id: Option<String>,
    delete_busy: bool,
    delete_error: String,
}

#[derive(Default)]
struct PollComposerState {
    open: bool,
    busy: bool,
    title: String,
    description: String,
    options: Vec<PollDraftOptionEntry>,
    is_quiz: bool,
    allows_multiple: bool,
    allows_revoting: bool,
    allows_user_options: bool,
    closes_automatically: bool,
    open_period_seconds: Option<i64>,
    duration_dialog_open: bool,
    duration_hours: String,
    duration_minutes: String,
    correct_option_id: Option<String>,
    error: String,
}

struct PollDraftOptionEntry {
    id: String,
    text: String,
}

#[derive(Clone)]
struct ChatSearchResultEntry {
    sender: String,
    timestamp: String,
    excerpt: String,
}

struct MessageEntry {
    id: String,
    author: String,
    author_tag: String,
    body: String,
    time_label: String,
    delivery_label: String,
    own: bool,
    edited: bool,
    day_label: String,
    show_day_label: bool,
    server_timestamp: i64,
    reactions: Vec<ReactionEntry>,
}

#[derive(Clone)]
struct ReactionEntry {
    actor_id: String,
    emoji: String,
    mine: bool,
}

struct PollState {
    item: chat_api::PollItem,
    metadata: Option<PollMetadataView>,
    extra_texts: HashMap<String, String>,
    selected: Vec<String>,
    proposal: String,
    error: String,
    busy: bool,
    owner: bool,
}

#[derive(Clone, Deserialize)]
struct PollMetadataView {
    title: String,
    description: String,
    options: Vec<PollOptionMetadataView>,
}

#[derive(Clone, Deserialize)]
struct PollOptionMetadataView {
    id: String,
    text: String,
}

struct LoadedConversation {
    detail: conversations_api::ConversationDetail,
    title: String,
    subtitle: String,
    peer_account_id: String,
    messages: Vec<MessageEntry>,
    polls: Vec<PollState>,
    pins: Vec<String>,
    history_cursor: Option<conversations_api::HistoryCursor>,
    history_has_older: bool,
    poll_cursor: Option<chat_api::PollHistoryRequest>,
    poll_has_older: bool,
    poll_error: String,
}

/// Load the conversation index or the conversation named by a deep-link route.
pub fn load(ctx: &AppContext, route: &str) {
    let Some(session) = ctx.active_session() else {
        ctx.apply_ui(|window| {
            window.set_conversations_loading(false);
            window.set_conversations_error("Войдите в аккаунт, чтобы открыть чаты.".into());
        });
        return;
    };

    let conversation_id = conversation_id_from_route(route);
    let has_chat_route = conversation_id.is_some();
    let account_id = session.account.id.clone();
    let route_for_request = route.to_owned();
    invalidate_navigation_requests(&account_id);
    let list_request_generation = begin_list_request(&account_id);
    if let Some(id) = conversation_id.clone() {
        let context = ctx.clone();
        let session_for_ui = session.clone();
        let account_id_for_ui = account_id.clone();
        let route_for_ui = route_for_request.clone();
        ctx.apply_ui(move |window| {
            if !list_request_is_current(
                &context,
                &window,
                &session_for_ui,
                &account_id_for_ui,
                &route_for_ui,
                list_request_generation,
            ) {
                return;
            }
            window.set_selected_conversation_id(id.into());
            window.set_chat_loading(true);
            window.set_chat_error_text("".into());
            window.set_polls_loading(true);
            window.set_polls_error_text("".into());
            window.set_messages(empty_model());
            window.set_polls(empty_model());
        });
    }

    let context = ctx.clone();
    let context_for_loading = context.clone();
    let session_for_loading = session.clone();
    let account_id_for_loading = account_id.clone();
    let route_for_loading = route_for_request.clone();
    ctx.apply_ui(move |window| {
        if !list_request_is_current(
            &context_for_loading,
            &window,
            &session_for_loading,
            &account_id_for_loading,
            &route_for_loading,
            list_request_generation,
        ) {
            return;
        }
        window.set_conversations_loading(true);
        window.set_conversations_error("".into());
    });

    let context_for_completion = context.clone();
    let session_for_completion = session.clone();
    let account_id_for_completion = account_id.clone();
    let requested_id = conversation_id.clone();
    let route_for_completion = route_for_request.clone();
    let result = ctx.call(
        "friendshub-conversations-list",
        |client| {
            let conversations = conversations_api::list(client)?;
            let folders = conversations_api::list_folders(client)?;
            Ok((conversations, folders))
        },
        move |window, result| {
            if !list_request_is_current(
                &context_for_completion,
                &window,
                &session_for_completion,
                &account_id_for_completion,
                &route_for_completion,
                list_request_generation,
            ) {
                return;
            }
            match result {
                Ok((conversations, folders)) => {
                    {
                        let mut state = state_lock();
                        let account = state
                            .accounts
                            .entry(account_id_for_completion.clone())
                            .or_default();
                        account.conversations = conversations;
                        account.folders = folders;
                    }
                    window.set_conversations_loading(false);
                    window.set_conversations_error("".into());
                    if let Some(id) = requested_id {
                        let _ =
                            load_conversation(&context, &id, false, Some(&route_for_completion));
                    }
                    apply_conversation_list(&context, window, &account_id_for_completion);
                }
                Err(error) => {
                    window.set_conversations_loading(false);
                    let message = error.user_message().to_owned();
                    window.set_conversations_error(message.clone().into());
                    if has_chat_route {
                        window.set_chat_loading(false);
                        window.set_chat_error_text(message.into());
                        window.set_polls_loading(false);
                    }
                }
            }
        },
    );
    if result.is_err() {
        let context = ctx.clone();
        let session_for_ui = session;
        let account_id_for_ui = account_id;
        let route_for_ui = route_for_request;
        ctx.apply_ui(move |window| {
            if !list_request_is_current(
                &context,
                &window,
                &session_for_ui,
                &account_id_for_ui,
                &route_for_ui,
                list_request_generation,
            ) {
                return;
            }
            window.set_conversations_loading(false);
            window.set_conversations_error("Не удалось запустить загрузку чатов.".into());
            if has_chat_route {
                window.set_chat_loading(false);
                window.set_chat_error_text("Не удалось загрузить историю чата.".into());
                window.set_polls_loading(false);
            }
        });
    }
}

/// Handle one action from the root UI dispatcher.
pub fn handle(
    ctx: &AppContext,
    namespace: &str,
    action: &str,
    arg1: &str,
    arg2: &str,
    _arg3: &str,
) -> bool {
    match namespace {
        "conversations" => handle_conversations(ctx, action, arg1, arg2),
        "chat" => handle_chat(ctx, action, arg1, arg2),
        "polls" => handle_polls(ctx, action, arg1, arg2),
        _ => false,
    }
}

/// Return the peer account ID for a loaded direct conversation.
///
/// Group, channel, and saved-message conversations have no peer profile target.
pub fn peer_account_id(ctx: &AppContext, conversation_id: &str) -> Option<String> {
    let account_id = ctx.active_session()?.account.id;
    let state = state_lock();
    let chat = state
        .accounts
        .get(&account_id)?
        .chats
        .get(conversation_id)?;
    (chat.kind == "direct" && !chat.peer_account_id.trim().is_empty())
        .then(|| chat.peer_account_id.clone())
}

fn handle_conversations(ctx: &AppContext, action: &str, arg1: &str, arg2: &str) -> bool {
    match action {
        "reload" => {
            let route = ctx
                .ui
                .upgrade()
                .map(|window| window.get_route().to_string())
                .unwrap_or_else(|| "/app".to_owned());
            load(ctx, &route);
            true
        }
        "search" => {
            let Some(account_id) = active_account_id(ctx) else {
                return true;
            };
            {
                let mut state = state_lock();
                state.accounts.entry(account_id.clone()).or_default().search = arg1.to_owned();
            }
            ctx.apply_ui({
                let query = arg1.to_owned();
                move |window| window.set_conversation_search(query.into())
            });
            apply_list_from_context(ctx, &account_id);
            true
        }
        "filter" => {
            let Some(account_id) = active_account_id(ctx) else {
                return true;
            };
            {
                let mut state = state_lock();
                let account = state.accounts.entry(account_id.clone()).or_default();
                if account.folders.iter().any(|folder| folder.id == arg1) {
                    account.selected_folder = arg1.to_owned();
                    account.selected_filter.clear();
                } else {
                    account.selected_filter = arg1.to_owned();
                    account.selected_folder.clear();
                }
            }
            apply_list_from_context(ctx, &account_id);
            true
        }
        "select" => {
            if arg1.is_empty() {
                return true;
            }
            let _ = load_conversation(ctx, arg1, true, None);
            true
        }
        "context-action" => {
            let conversation_id = arg1.to_owned();
            match arg2 {
                "mark-read" => mark_conversation_read(ctx, &conversation_id, None),
                "toggle-archive" => toggle_archive(ctx, &conversation_id),
                "toggle-mute" => toggle_mute(ctx, &conversation_id),
                "delete" => leave_conversation(ctx, &conversation_id),
                _ => return false,
            }
            true
        }
        // These UI callbacks contain no folder name, edited name, or ordering.
        // Consume them without sending an invented request body.
        "folder-create" | "folder-edit" | "create-temporary" => true,
        _ => false,
    }
}

fn handle_chat(ctx: &AppContext, action: &str, arg1: &str, arg2: &str) -> bool {
    match action {
        "send" => {
            send_text(ctx, arg1);
            true
        }
        "draft" => {
            if let Some(account_id) = active_account_id(ctx)
                && let Some(conversation_id) = selected_conversation_id(ctx)
            {
                let mut state = state_lock();
                let chat = state
                    .accounts
                    .entry(account_id)
                    .or_default()
                    .chats
                    .entry(conversation_id)
                    .or_default();
                chat.draft = arg1.to_owned();
            }
            apply_selected_chat(ctx);
            true
        }
        "search-toggle" => {
            if let Some(account_id) = active_account_id(ctx)
                && let Some(conversation_id) = selected_conversation_id(ctx)
            {
                if let Some(chat) = state_lock()
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conversation_id))
                {
                    chat.search_open = !chat.search_open;
                }
                apply_selected_chat(ctx);
            }
            true
        }
        "search" => {
            if let Some(account_id) = active_account_id(ctx)
                && let Some(conversation_id) = selected_conversation_id(ctx)
            {
                if let Some(chat) = state_lock()
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conversation_id))
                {
                    update_chat_search(chat, arg1);
                }
                apply_selected_chat(ctx);
            }
            true
        }
        "search-previous" | "search-next" => {
            if let Some(account_id) = active_account_id(ctx)
                && let Some(conversation_id) = selected_conversation_id(ctx)
            {
                if let Some(chat) = state_lock()
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conversation_id))
                {
                    move_search_selection(chat, action == "search-next");
                }
                apply_selected_chat(ctx);
            }
            true
        }
        "search-result" => {
            let Some(index) = arg1.parse::<i32>().ok() else {
                return true;
            };
            if let Some(account_id) = active_account_id(ctx)
                && let Some(conversation_id) = selected_conversation_id(ctx)
            {
                if let Some(chat) = state_lock()
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conversation_id))
                    && index >= 0
                    && (index as usize) < chat.search_results.len()
                {
                    chat.search_selected_index = index;
                }
                apply_selected_chat(ctx);
            }
            true
        }
        "cancel-edit" => {
            if let Some(account_id) = active_account_id(ctx)
                && let Some(conversation_id) = selected_conversation_id(ctx)
                && let Some(chat) = state_lock()
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conversation_id))
            {
                chat.editing = false;
                chat.editing_message_id = None;
                chat.draft.clear();
                chat.composer_error.clear();
                apply_selected_chat(ctx);
            }
            true
        }
        "message-menu" => {
            open_message_menu(ctx, arg1);
            true
        }
        "message-menu-close" => {
            close_message_menu(ctx);
            true
        }
        "message-menu-action" => {
            select_message_menu_action(ctx, arg1, arg2);
            true
        }
        "delete-confirm" => {
            confirm_selected_message_delete(ctx);
            true
        }
        "delete-cancel" => {
            cancel_message_delete(ctx);
            true
        }
        "message-menu-reaction" => {
            if let Ok(index) = arg1.parse::<usize>() {
                set_reaction(ctx, index, arg2);
            }
            true
        }
        "load-older" => {
            if let Some(conversation_id) = selected_conversation_id(ctx) {
                let _ = load_older_messages(ctx, &conversation_id);
            }
            true
        }
        "reaction" => {
            let Some(index) = arg1.parse::<usize>().ok() else {
                return true;
            };
            set_reaction(ctx, index, arg2);
            true
        }
        "pins-open" | "pins-previous" | "pins-next" => {
            move_pin_selection(ctx, action);
            true
        }
        "pin-remove" => {
            unpin_current(ctx);
            true
        }
        // The native core wrappers list and cancel scheduled messages but do
        // not expose scheduled sending.
        "schedule" => false,
        _ => false,
    }
}

fn open_message_menu(ctx: &AppContext, index_text: &str) {
    let Ok(index) = index_text.parse::<usize>() else {
        return;
    };
    let Some(account_id) = active_account_id(ctx) else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let mut state = state_lock();
    if let Some(chat) = state
        .accounts
        .get_mut(&account_id)
        .and_then(|account| account.chats.get_mut(&conversation_id))
        && index < chat.messages.len()
    {
        chat.message_menu_open = true;
        chat.message_menu_index = index as i32;
    }
    drop(state);
    apply_selected_chat(ctx);
}

fn close_message_menu(ctx: &AppContext) {
    if let Some(account_id) = active_account_id(ctx)
        && let Some(conversation_id) = selected_conversation_id(ctx)
        && let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(&conversation_id))
    {
        chat.message_menu_open = false;
        chat.message_menu_index = -1;
    }
    apply_selected_chat(ctx);
}

fn select_message_menu_action(ctx: &AppContext, index_text: &str, action: &str) {
    let Ok(index) = index_text.parse::<usize>() else {
        close_message_menu(ctx);
        return;
    };
    let Some(account_id) = active_account_id(ctx) else {
        close_message_menu(ctx);
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        close_message_menu(ctx);
        return;
    };
    let selected = {
        let state = state_lock();
        state
            .accounts
            .get(&account_id)
            .and_then(|account| account.chats.get(&conversation_id))
            .and_then(|chat| chat.messages.get(index))
            .map(|message| (message.id.clone(), message.body.clone(), message.own))
    };
    close_message_menu(ctx);
    let Some((message_id, body, own)) = selected else {
        return;
    };
    match action {
        "edit" if own => {
            if let Some(chat) = state_lock()
                .accounts
                .get_mut(&account_id)
                .and_then(|account| account.chats.get_mut(&conversation_id))
            {
                chat.editing = true;
                chat.editing_message_id = Some(message_id);
                chat.draft = body;
                chat.composer_error.clear();
            }
            apply_selected_chat(ctx);
        }
        "delete" if own => request_message_delete_confirmation(ctx, &conversation_id, &message_id),
        _ => {}
    }
}

fn request_message_delete_confirmation(ctx: &AppContext, conversation_id: &str, message_id: &str) {
    let Some(account_id) = active_account_id(ctx) else {
        return;
    };
    if let Some(chat) = state_lock()
        .accounts
        .get_mut(&account_id)
        .and_then(|account| account.chats.get_mut(conversation_id))
        && chat
            .messages
            .iter()
            .any(|message| message.id == message_id && message.own)
    {
        chat.delete_confirmation_message_id = Some(message_id.to_owned());
        chat.delete_error.clear();
        chat.message_menu_open = false;
        chat.message_menu_index = -1;
        apply_selected_chat(ctx);
    }
}

fn confirm_selected_message_delete(ctx: &AppContext) {
    let Some(account_id) = active_account_id(ctx) else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let message_id = {
        let state = state_lock();
        state
            .accounts
            .get(&account_id)
            .and_then(|account| account.chats.get(&conversation_id))
            .and_then(|chat| {
                (!chat.delete_busy)
                    .then(|| chat.delete_confirmation_message_id.clone())
                    .flatten()
            })
    };
    if let Some(message_id) = message_id {
        delete_message(ctx, &conversation_id, &message_id);
    }
}

fn cancel_message_delete(ctx: &AppContext) {
    let Some(account_id) = active_account_id(ctx) else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    if let Some(chat) = state_lock()
        .accounts
        .get_mut(&account_id)
        .and_then(|account| account.chats.get_mut(&conversation_id))
        && !chat.delete_busy
    {
        chat.delete_confirmation_message_id = None;
        chat.delete_error.clear();
        apply_selected_chat(ctx);
    }
}

fn delete_message(ctx: &AppContext, conversation_id: &str, message_id: &str) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let account_id = session.account.id.clone();
    let is_own_message = {
        let state = state_lock();
        state
            .accounts
            .get(&account_id)
            .and_then(|account| account.chats.get(conversation_id))
            .and_then(|chat| {
                chat.messages
                    .iter()
                    .find(|message| message.id == message_id)
            })
            .is_some_and(|message| message.own)
    };
    if !is_own_message {
        return;
    }
    let request_key = format!("delete:{message_id}");
    let request_generation = next_request_generation();
    {
        let mut state = state_lock();
        let Some(chat) = state
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        else {
            return;
        };
        if chat.busy || chat.loading || chat.delete_busy {
            return;
        }
        chat.busy = true;
        chat.delete_busy = true;
        chat.delete_error.clear();
        chat.request_generations
            .insert(request_key.clone(), request_generation);
    }
    apply_selected_chat(ctx);

    let request = chat_api::DeleteMessageRequest {
        conversation_id: conversation_id.to_owned(),
        target_message_id: message_id.to_owned(),
    };
    let account_id_for_ui = account_id.clone();
    let conversation_id_for_ui = conversation_id.to_owned();
    let message_id_for_ui = message_id.to_owned();
    let request_key_for_error = request_key.clone();
    let session_for_completion = session.clone();
    let session_for_error = session.clone();
    let context = ctx.clone();
    let result = ctx.call(
        "friendshub-message-delete",
        move |client| chat_api::delete_message(client, &request),
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !chat_request_generation_is_current(
                    &account_id_for_ui,
                    &conversation_id_for_ui,
                    &request_key,
                    request_generation,
                )
            {
                return;
            }
            let visible = is_chat_route_visible(&window, &conversation_id_for_ui, false, None);
            let mut state = state_lock();
            if let Some(chat) = state
                .accounts
                .entry(account_id_for_ui)
                .or_default()
                .chats
                .get_mut(&conversation_id_for_ui)
            {
                chat.busy = false;
                match result {
                    Ok(_) => {
                        chat.messages
                            .retain(|message| message.id != message_id_for_ui);
                        chat.pins.retain(|id| id != &message_id_for_ui);
                        chat.selected_pin =
                            chat.selected_pin.min(chat.pins.len().saturating_sub(1));
                        if chat.editing_message_id.as_deref() == Some(message_id_for_ui.as_str()) {
                            chat.editing = false;
                            chat.editing_message_id = None;
                            chat.draft.clear();
                        }
                        if chat.delete_confirmation_message_id.as_deref()
                            == Some(message_id_for_ui.as_str())
                        {
                            chat.delete_confirmation_message_id = None;
                        }
                        chat.delete_error.clear();
                        if !chat.search_query.trim().is_empty() {
                            let query = chat.search_query.clone();
                            update_chat_search(chat, &query);
                        }
                        chat.error.clear();
                    }
                    Err(error) => chat.delete_error = error.user_message().to_owned(),
                }
                chat.delete_busy = false;
                if visible {
                    apply_chat_models(window, chat);
                }
            }
        },
    );
    if let Err(error) = result {
        if !same_active_session(ctx, &session_for_error)
            || !chat_request_generation_is_current(
                &account_id,
                conversation_id,
                &request_key_for_error,
                request_generation,
            )
        {
            return;
        }
        if let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        {
            chat.busy = false;
            chat.delete_busy = false;
            chat.delete_error = error.to_string();
        }
        apply_selected_chat(ctx);
    }
}

fn handle_polls(ctx: &AppContext, action: &str, arg1: &str, arg2: &str) -> bool {
    match action {
        "retry" => {
            if let Some(conversation_id) = selected_conversation_id(ctx) {
                let _ = reload_polls(ctx, &conversation_id);
            }
            true
        }
        "load-older" => {
            if let Some(conversation_id) = selected_conversation_id(ctx) {
                let _ = load_older_polls(ctx, &conversation_id);
            }
            true
        }
        "select-option" => {
            select_poll_option(ctx, arg1, arg2);
            true
        }
        "vote" => {
            vote_poll(ctx, arg1);
            true
        }
        "proposal-changed" => {
            if let Some(account_id) = active_account_id(ctx)
                && let Some(conversation_id) = selected_conversation_id(ctx)
            {
                let mut state = state_lock();
                let chat = state
                    .accounts
                    .entry(account_id)
                    .or_default()
                    .chats
                    .entry(conversation_id)
                    .or_default();
                chat.poll_proposals.insert(arg1.to_owned(), arg2.to_owned());
                if let Some(poll) = chat.polls.iter_mut().find(|poll| poll.item.id == arg1) {
                    poll.proposal = arg2.to_owned();
                }
            }
            true
        }
        "add-option" => {
            add_poll_option(ctx, arg1, arg2);
            true
        }
        "remove-option" => {
            remove_poll_option(ctx, arg1, arg2);
            true
        }
        "stop" => {
            mutate_poll(ctx, arg1, PollMutation::Stop);
            true
        }
        "delete" => {
            mutate_poll(ctx, arg1, PollMutation::Delete);
            true
        }
        "create-dialog" => {
            open_poll_composer(ctx);
            true
        }
        "title-changed" | "description-changed" => {
            update_poll_composer_text(ctx, action, arg1);
            true
        }
        "type-changed"
        | "multiple-changed"
        | "revoting-changed"
        | "user-options-changed"
        | "auto-close-changed" => {
            update_poll_composer_toggle(ctx, action, parse_bool(arg1));
            true
        }
        "duration-requested" => {
            set_poll_duration_dialog(ctx, true);
            true
        }
        "duration-selected" => {
            select_poll_duration(ctx, arg1, arg2);
            true
        }
        "duration-cancelled" => {
            set_poll_duration_dialog(ctx, false);
            true
        }
        "option-changed" => {
            if let Ok(index) = arg1.parse::<usize>() {
                update_poll_option_text(ctx, index, arg2);
            }
            true
        }
        "option-correct" => {
            if let Ok(index) = arg1.parse::<usize>() {
                set_poll_correct_option(ctx, index);
            }
            true
        }
        "option-remove" => {
            if let Ok(index) = arg1.parse::<usize>() {
                remove_poll_draft_option(ctx, index);
            }
            true
        }
        "option-add" => {
            add_poll_draft_option(ctx);
            true
        }
        "create" => {
            create_poll(ctx);
            true
        }
        "close" => {
            close_poll_composer(ctx);
            true
        }
        _ => false,
    }
}

fn new_poll_draft_option() -> PollDraftOptionEntry {
    PollDraftOptionEntry {
        id: uuid_v7().replace('-', ""),
        text: String::new(),
    }
}

fn reset_poll_composer(open: bool) -> PollComposerState {
    let first = new_poll_draft_option();
    let second = new_poll_draft_option();
    PollComposerState {
        open,
        busy: false,
        title: String::new(),
        description: String::new(),
        options: vec![first, second],
        is_quiz: false,
        allows_multiple: false,
        allows_revoting: true,
        allows_user_options: false,
        closes_automatically: false,
        open_period_seconds: None,
        duration_dialog_open: false,
        duration_hours: "1".to_owned(),
        duration_minutes: "0".to_owned(),
        correct_option_id: None,
        error: String::new(),
    }
}

fn open_poll_composer(ctx: &AppContext) {
    let Some(account_id) = active_account_id(ctx) else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    if let Some(chat) = state_lock()
        .accounts
        .get_mut(&account_id)
        .and_then(|account| account.chats.get_mut(&conversation_id))
        && chat.kind == "group"
    {
        if chat.poll_composer.options.is_empty() {
            chat.poll_composer = reset_poll_composer(true);
        } else {
            chat.poll_composer.open = true;
            chat.poll_composer.error.clear();
        }
        apply_selected_chat(ctx);
    }
}

fn close_poll_composer(ctx: &AppContext) {
    update_poll_composer(ctx, |composer| {
        if composer.busy {
            return;
        }
        composer.open = false;
        composer.duration_dialog_open = false;
    });
}

fn set_poll_duration_dialog(ctx: &AppContext, open: bool) {
    update_poll_composer(ctx, |composer| {
        composer.duration_dialog_open = open;
        if open && let Some(seconds) = composer.open_period_seconds {
            composer.duration_hours = (seconds / 3600).to_string();
            composer.duration_minutes = ((seconds % 3600) / 60).to_string();
        }
    });
}

fn update_poll_composer_text(ctx: &AppContext, action: &str, value: &str) {
    let value = match action {
        "title-changed" => truncate_chars(value, 64),
        "description-changed" => truncate_chars(value, 128),
        _ => return,
    };
    update_poll_composer(ctx, move |composer| {
        if action == "title-changed" {
            composer.title = value;
        } else {
            composer.description = value;
        }
        composer.error.clear();
    });
}

fn update_poll_composer_toggle(ctx: &AppContext, action: &str, value: bool) {
    update_poll_composer(ctx, move |composer| match action {
        "type-changed" => {
            composer.is_quiz = value;
            if value {
                composer.allows_multiple = false;
                if composer
                    .correct_option_id
                    .as_ref()
                    .is_none_or(|id| !composer.options.iter().any(|option| &option.id == id))
                {
                    composer.correct_option_id =
                        composer.options.first().map(|option| option.id.clone());
                }
            }
        }
        "multiple-changed" => composer.allows_multiple = value && !composer.is_quiz,
        "revoting-changed" => composer.allows_revoting = value,
        "user-options-changed" => composer.allows_user_options = value,
        "auto-close-changed" => {
            composer.closes_automatically = value;
            composer.open_period_seconds = if value {
                Some(composer.open_period_seconds.unwrap_or(3600))
            } else {
                None
            };
        }
        _ => {}
    });
}

fn select_poll_duration(ctx: &AppContext, hours_text: &str, minutes_text: &str) {
    let duration = poll_duration_seconds(hours_text, minutes_text);
    update_poll_composer(ctx, move |composer| match duration {
        Some(seconds) if valid_duration_fields(hours_text, minutes_text) => {
            composer.open_period_seconds = Some(seconds);
            composer.duration_hours = hours_text.to_owned();
            composer.duration_minutes = minutes_text.to_owned();
            composer.duration_dialog_open = false;
            composer.error.clear();
        }
        _ => composer.error = "Укажите время от 1 минуты до 24 часов.".to_owned(),
    });
}

fn update_poll_option_text(ctx: &AppContext, index: usize, text: &str) {
    let text = truncate_chars(text, 128);
    update_poll_composer(ctx, move |composer| {
        if let Some(option) = composer.options.get_mut(index) {
            option.text = text;
            composer.error.clear();
        }
    });
}

fn set_poll_correct_option(ctx: &AppContext, index: usize) {
    update_poll_composer(ctx, move |composer| {
        if let Some(option) = composer.options.get(index) {
            composer.correct_option_id = Some(option.id.clone());
        }
    });
}

fn add_poll_draft_option(ctx: &AppContext) {
    update_poll_composer(ctx, |composer| {
        if composer.options.len() < 15 {
            composer.options.push(new_poll_draft_option());
        }
    });
}

fn remove_poll_draft_option(ctx: &AppContext, index: usize) {
    update_poll_composer(ctx, move |composer| {
        if composer.options.len() <= 2 || index >= composer.options.len() {
            return;
        }
        let removed = composer.options.remove(index);
        if composer.correct_option_id.as_deref() == Some(removed.id.as_str()) {
            composer.correct_option_id = composer.options.first().map(|option| option.id.clone());
        }
    });
}

fn update_poll_composer(ctx: &AppContext, update: impl FnOnce(&mut PollComposerState)) {
    let Some(account_id) = active_account_id(ctx) else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    {
        let mut state = state_lock();
        let Some(chat) = state
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(&conversation_id))
        else {
            return;
        };
        update(&mut chat.poll_composer);
    }
    apply_selected_chat(ctx);
}

fn poll_composer_valid(composer: &PollComposerState) -> bool {
    let ids = composer
        .options
        .iter()
        .map(|option| option.id.as_str())
        .collect::<HashSet<_>>();
    composer.title.trim().chars().count() > 0
        && composer.title.chars().count() <= 64
        && composer.description.chars().count() <= 128
        && (2..=15).contains(&composer.options.len())
        && ids.len() == composer.options.len()
        && composer.options.iter().all(|option| {
            (1..=64).contains(&option.id.chars().count())
                && option.text.trim().chars().count() > 0
                && option.text.chars().count() <= 128
        })
        && (!composer.is_quiz
            || composer
                .correct_option_id
                .as_ref()
                .is_some_and(|id| composer.options.iter().any(|option| &option.id == id)))
        && (!composer.closes_automatically
            || composer
                .open_period_seconds
                .is_some_and(|seconds| (60..=86_400).contains(&seconds)))
}

fn valid_duration_fields(hours: &str, minutes: &str) -> bool {
    poll_duration_seconds(hours, minutes).is_some()
}

fn poll_duration_seconds(hours: &str, minutes: &str) -> Option<i64> {
    let hours = hours.parse::<i64>().ok()?;
    let minutes = minutes.parse::<i64>().ok()?;
    if !(0..=24).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    let seconds = hours.checked_mul(3600).and_then(|seconds| {
        minutes
            .checked_mul(60)
            .and_then(|minutes| seconds.checked_add(minutes))
    })?;
    (60..=86_400).contains(&seconds).then_some(seconds)
}

fn parse_bool(value: &str) -> bool {
    value == "true"
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

fn create_poll(ctx: &AppContext) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let account_id = session.account.id.clone();
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let request_key = "poll-create".to_owned();
    let request_generation = next_request_generation();
    let request = {
        let mut state = state_lock();
        let Some(chat) = state
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(&conversation_id))
        else {
            return;
        };
        if chat.kind != "group" || chat.loading || chat.busy || chat.poll_composer.busy {
            return;
        }
        if !poll_composer_valid(&chat.poll_composer) {
            chat.poll_composer.error = localized(
                "Проверьте вопрос и варианты ответа.",
                "Check the question and answer options.",
            )
            .to_owned();
            drop(state);
            apply_selected_chat(ctx);
            return;
        }

        let composer = &mut chat.poll_composer;
        composer.busy = true;
        composer.error.clear();
        chat.request_generations
            .insert(request_key.clone(), request_generation);
        chat_api::CreateConversationPollRequest {
            conversation_id: conversation_id.clone(),
            metadata: chat_api::PollMetadata {
                title: composer.title.trim().to_owned(),
                description: composer.description.trim().to_owned(),
                options: composer
                    .options
                    .iter()
                    .map(|option| chat_api::PollOption {
                        id: option.id.clone(),
                        text: option.text.trim().to_owned(),
                    })
                    .collect(),
            },
            settings: chat_api::PollSettings {
                poll_type: if composer.is_quiz {
                    chat_api::PollType::Quiz
                } else {
                    chat_api::PollType::Regular
                },
                allows_multiple: composer.allows_multiple && !composer.is_quiz,
                allows_revoting: composer.allows_revoting,
                allows_user_options: composer.allows_user_options,
                open_period_seconds: composer
                    .closes_automatically
                    .then_some(composer.open_period_seconds)
                    .flatten(),
                correct_option_id: composer
                    .is_quiz
                    .then(|| composer.correct_option_id.clone())
                    .flatten(),
            },
        }
    };
    apply_selected_chat(ctx);

    let session_for_completion = session.clone();
    let session_for_error = session.clone();
    let account_id_for_ui = account_id.clone();
    let conversation_id_for_ui = conversation_id.clone();
    let request_key_for_error = request_key.clone();
    let context = ctx.clone();
    let result = ctx.call(
        "friendshub-poll-create",
        move |client| chat_api::create_conversation_poll(client, &request),
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !chat_request_generation_is_current(
                    &account_id_for_ui,
                    &conversation_id_for_ui,
                    &request_key,
                    request_generation,
                )
            {
                return;
            }
            let visible = is_chat_route_visible(&window, &conversation_id_for_ui, false, None);
            let mut state = state_lock();
            let Some(chat) = state
                .accounts
                .entry(account_id_for_ui)
                .or_default()
                .chats
                .get_mut(&conversation_id_for_ui)
            else {
                return;
            };
            chat.poll_composer.busy = false;
            match result {
                Ok(created) => {
                    let metadata = PollMetadataView {
                        title: created.metadata.title,
                        description: created.metadata.description,
                        options: created
                            .metadata
                            .options
                            .into_iter()
                            .map(|option| PollOptionMetadataView {
                                id: option.id,
                                text: option.text,
                            })
                            .collect(),
                    };
                    let poll = PollState {
                        owner: created.item.creator_account_id.as_deref()
                            == Some(session_for_completion.account.id.as_str()),
                        selected: created.item.my_votes.clone(),
                        item: created.item,
                        metadata: Some(metadata),
                        extra_texts: HashMap::new(),
                        proposal: String::new(),
                        error: String::new(),
                        busy: false,
                    };
                    if !chat.polls.iter().any(|existing| existing.item.id == poll.item.id) {
                        chat.polls.push(poll);
                        chat.polls.sort_by(|left, right| {
                            left.item.created_at
                                .cmp(&right.item.created_at)
                                .then_with(|| left.item.id.cmp(&right.item.id))
                        });
                    }
                    chat.poll_composer = reset_poll_composer(false);
                    chat.banner_text = if created.history_synced {
                        String::new()
                    } else {
                        localized(
                            "Опрос создан, но его сообщение не попало в историю на этом устройстве.",
                            "The poll was created, but its history message was not sent from this device.",
                        )
                        .to_owned()
                    };
                    chat.polls_error.clear();
                }
                Err(error) => {
                    chat.poll_composer.error = error.user_message().to_owned();
                }
            }
            if visible {
                apply_chat_models(window, chat);
            }
        },
    );
    if let Err(error) = result {
        if !same_active_session(ctx, &session_for_error)
            || !chat_request_generation_is_current(
                &account_id,
                &conversation_id,
                &request_key_for_error,
                request_generation,
            )
        {
            return;
        }
        if let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(&conversation_id))
        {
            chat.poll_composer.busy = false;
            chat.poll_composer.error = error.to_string();
        }
        apply_selected_chat(ctx);
    }
}

fn load_conversation(
    ctx: &AppContext,
    conversation_id: &str,
    choose_route: bool,
    expected_route: Option<&str>,
) -> std::io::Result<()> {
    let Some(session) = ctx.active_session() else {
        return Ok(());
    };
    let account_id = session.account.id.clone();
    let request_generation = next_request_generation();
    let initial_draft = {
        let mut state = state_lock();
        let account = state.accounts.entry(account_id.clone()).or_default();
        account.request_generations.clear();
        for (id, other_chat) in &mut account.chats {
            if id != conversation_id {
                other_chat.request_generations.remove("load");
            }
        }
        let chat = account.chats.entry(conversation_id.to_owned()).or_default();
        chat.loading = true;
        chat.busy = false;
        chat.error.clear();
        chat.composer_error.clear();
        chat.editing = false;
        chat.editing_message_id = None;
        chat.message_menu_open = false;
        chat.message_menu_index = -1;
        chat.delete_confirmation_message_id = None;
        chat.delete_busy = false;
        chat.delete_error.clear();
        chat.poll_composer.open = false;
        chat.poll_composer.duration_dialog_open = false;
        chat.poll_composer.busy = false;
        chat.polls_loading = true;
        chat.polls_loading_older = false;
        chat.polls_error.clear();
        chat.history_loading_older = false;
        chat.search_open = false;
        chat.search_query.clear();
        chat.search_results.clear();
        chat.search_selected_index = -1;
        chat.request_generations.retain(|key, _| {
            !matches!(
                key.as_str(),
                "load" | "history-older" | "polls-reload" | "polls-older"
            )
        });
        chat.request_generations
            .insert("load".to_owned(), request_generation);
        chat.draft.clone()
    };

    let selected_id = conversation_id.to_owned();
    let route_for_ui = expected_route.map(str::to_owned);
    let session_for_ui = session.clone();
    let context_for_ui = ctx.clone();
    let account_id_for_ui = account_id.clone();
    let conversation_id_for_ui = selected_id.clone();
    ctx.apply_ui(move |window| {
        if !same_active_session(&context_for_ui, &session_for_ui)
            || !is_chat_route_visible(
                &window,
                &conversation_id_for_ui,
                choose_route,
                route_for_ui.as_deref(),
            )
        {
            return;
        }
        if !chat_request_generation_is_current(
            &account_id_for_ui,
            &conversation_id_for_ui,
            "load",
            request_generation,
        ) {
            return;
        }
        window.set_selected_conversation_id(selected_id.into());
        window.set_conversations_error("".into());
        window.set_chat_loading(true);
        window.set_chat_error_text("".into());
        window.set_chat_composer_error("".into());
        window.set_chat_busy(false);
        window.set_chat_editing(false);
        window.set_chat_composer_enabled(false);
        window.set_chat_has_older(false);
        window.set_chat_banner_text("".into());
        window.set_chat_pinned_text("".into());
        window.set_chat_pin_count(0);
        window.set_chat_pin_counter("".into());
        window.set_chat_search_open(false);
        window.set_chat_search_query("".into());
        window.set_chat_search_results(empty_model());
        window.set_chat_searching(false);
        window.set_chat_search_selected_index(-1);
        window.set_chat_search_position("".into());
        window.set_chat_can_call(false);
        window.set_chat_can_poll(false);
        window.set_chat_can_schedule(false);
        window.set_chat_delete_confirm_open(false);
        window.set_chat_delete_confirm_busy(false);
        window.set_chat_delete_confirm_error_text("".into());
        window.set_polls_loading(true);
        window.set_polls_has_older(false);
        window.set_polls_loading_older(false);
        window.set_polls_error_text("".into());
        window.set_chat_draft(initial_draft.into());
        window.set_messages(empty_model());
        window.set_polls(empty_model());
    });

    let id = conversation_id.to_owned();
    let id_for_completion = id.clone();
    let session_for_completion = session.clone();
    let session_for_error = session.clone();
    let account = session.account;
    let account_id_for_completion = account_id.clone();
    let context = ctx.clone();
    let expected_route_for_completion = expected_route.map(str::to_owned);
    let result = ctx.call(
        "friendshub-chat-history",
        move |client| load_conversation_data(client, &id, &account),
        move |window, result| match result {
            Ok(loaded) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id_for_completion,
                        &id_for_completion,
                        "load",
                        request_generation,
                    )
                {
                    return;
                }
                let visible = is_chat_route_visible(
                    &window,
                    &id_for_completion,
                    choose_route,
                    expected_route_for_completion.as_deref(),
                );
                let mark_as_read = matches!(loaded.detail.kind.as_str(), "direct" | "group");
                let screen = if loaded.detail.kind == "channel" {
                    "channel"
                } else {
                    "chat"
                };
                let route = if screen == "channel" {
                    format!("/app/channel/{id_for_completion}")
                } else {
                    format!("/app/chat/{id_for_completion}")
                };
                let mut state = state_lock();
                let account = state
                    .accounts
                    .entry(account_id_for_completion.clone())
                    .or_default();
                if let Some(list_item) = account
                    .conversations
                    .iter_mut()
                    .find(|item| item.id == id_for_completion)
                {
                    list_item.title = Some(loaded.title.clone());
                }
                let banner_text = conversation_banner(&account.conversations, &id_for_completion);
                let chat = account.chats.entry(id_for_completion.clone()).or_default();
                chat.title = loaded.title;
                chat.subtitle = loaded.subtitle;
                chat.kind = loaded.detail.kind;
                chat.peer_account_id = loaded.peer_account_id;
                chat.messages = loaded.messages;
                chat.polls = loaded.polls;
                chat.pins = loaded.pins;
                chat.selected_pin = 0;
                chat.history_cursor = loaded.history_cursor;
                chat.history_has_older = loaded.history_has_older;
                chat.history_loading_older = false;
                chat.poll_cursor = loaded.poll_cursor;
                chat.poll_has_older = loaded.poll_has_older;
                chat.polls_loading = false;
                chat.polls_loading_older = false;
                chat.polls_error = loaded.poll_error;
                chat.banner_text = banner_text;
                chat.loading = false;
                chat.error.clear();
                if visible {
                    window.set_selected_conversation_id(id_for_completion.clone().into());
                    window.set_conversation_kind(chat.kind.clone().into());
                    window.set_conversation_title(chat.title.clone().into());
                    window.set_conversation_subtitle(chat.subtitle.clone().into());
                    if choose_route {
                        window.set_route(route.into());
                        window.set_screen(screen.into());
                    }
                    apply_chat_models(window, chat);
                }
                drop(state);
                if visible
                    && let Some(window) = context.ui.upgrade()
                    && is_conversation_screen(&window)
                    && same_active_session(&context, &session_for_completion)
                {
                    apply_conversation_list(&context, window, &account_id_for_completion);
                }
                if visible && mark_as_read {
                    mark_conversation_read(&context, &id_for_completion, None);
                }
            }
            Err(error) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id_for_completion,
                        &id_for_completion,
                        "load",
                        request_generation,
                    )
                {
                    return;
                }
                let visible = is_chat_route_visible(
                    &window,
                    &id_for_completion,
                    choose_route,
                    expected_route_for_completion.as_deref(),
                );
                let message = error.user_message().to_owned();
                if let Some(account) = state_lock().accounts.get_mut(&account_id_for_completion) {
                    if let Some(chat) = account.chats.get_mut(&id_for_completion) {
                        chat.loading = false;
                        chat.polls_loading = false;
                        chat.error = message.clone();
                    }
                }
                if visible {
                    window.set_chat_loading(false);
                    window.set_polls_loading(false);
                    window.set_chat_error_text(message.into());
                }
            }
        },
    );
    if let Err(error) = result {
        if !same_active_session(ctx, &session_for_error)
            || !chat_request_generation_is_current(
                &account_id,
                conversation_id,
                "load",
                request_generation,
            )
        {
            return Ok(());
        }
        if let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        {
            chat.loading = false;
            chat.polls_loading = false;
            chat.error = error.to_string();
        }
        let message = error.to_string();
        let session_for_ui = session_for_error;
        let context_for_ui = ctx.clone();
        let account_id_for_ui = account_id.clone();
        let conversation_id_for_ui = conversation_id.to_owned();
        let route_for_ui = expected_route.map(str::to_owned);
        ctx.apply_ui(move |window| {
            if !same_active_session(&context_for_ui, &session_for_ui)
                || !chat_request_generation_is_current(
                    &account_id_for_ui,
                    &conversation_id_for_ui,
                    "load",
                    request_generation,
                )
                || !is_chat_route_visible(
                    &window,
                    &conversation_id_for_ui,
                    choose_route,
                    route_for_ui.as_deref(),
                )
            {
                return;
            }
            window.set_chat_loading(false);
            window.set_polls_loading(false);
            window.set_chat_error_text(message.into());
        });
        return Err(error);
    }
    Ok(())
}

fn load_conversation_data(
    client: &friendshub_core::Client,
    conversation_id: &str,
    account: &crate::platform::accounts::LocalAccount,
) -> Result<LoadedConversation, CoreError> {
    let detail = conversations_api::get(client, conversation_id)?;
    let peer_account_id = if detail.kind == "direct" {
        detail
            .members
            .iter()
            .find(|member| member.as_str() != account.id)
            .cloned()
            .unwrap_or_default()
    } else {
        String::new()
    };
    let peer_profile = if peer_account_id.is_empty() {
        None
    } else {
        profile_settings::get_public_profile(
            client,
            &profile_settings::GetPublicProfileRequest {
                id: peer_account_id.clone(),
            },
        )
        .ok()
    };
    let title = detail
        .title
        .clone()
        .or_else(|| {
            peer_profile
                .as_ref()
                .map(|profile| profile.username.clone())
        })
        .unwrap_or_else(|| fallback_conversation_title(&detail.kind, &detail.id));
    let subtitle = if detail.kind == "direct" {
        peer_profile
            .as_ref()
            .map(|profile| profile.fh_number.clone())
            .unwrap_or_default()
    } else {
        format!("{} участника", detail.members.len())
    };

    let (messages, history_cursor, history_has_older, newest_timestamp) = if detail.kind == "direct"
    {
        let rows = conversations_api::envelopes(
            client,
            conversation_id,
            conversations_api::HistoryCursor {
                before: None,
                before_id: None,
                limit: Some(HISTORY_PAGE_SIZE),
            },
        )?;
        let newest_timestamp = rows.iter().map(|row| row.server_timestamp).max();
        let history_cursor = rows
            .iter()
            .min_by(|left, right| {
                left.server_timestamp
                    .cmp(&right.server_timestamp)
                    .then_with(|| left.envelope_id.cmp(&right.envelope_id))
            })
            .map(|row| conversations_api::HistoryCursor {
                before: Some(row.server_timestamp),
                before_id: Some(row.envelope_id.clone()),
                limit: Some(HISTORY_PAGE_SIZE),
            });
        let has_older = rows.len() as i64 >= HISTORY_PAGE_SIZE;
        let mut ordered = rows;
        ordered.sort_by(|left, right| {
            left.server_timestamp
                .cmp(&right.server_timestamp)
                .then_with(|| left.envelope_id.cmp(&right.envelope_id))
        });
        let messages = decrypt_direct_history(client, &ordered, conversation_id, account)?;
        (messages, history_cursor, has_older, newest_timestamp)
    } else {
        (Vec::new(), None, false, None)
    };

    let (polls, poll_error) = match load_poll_page(client, conversation_id, account, None, None) {
        Ok(polls) => (polls, String::new()),
        Err(error) => (Vec::new(), error.to_string()),
    };
    let poll_cursor = polls.first().map(|poll| chat_api::PollHistoryRequest {
        conversation_id: conversation_id.to_owned(),
        before: Some(poll.item.created_at),
        before_id: Some(poll.item.id.clone()),
        limit: Some(POLL_PAGE_SIZE),
    });
    let poll_has_older = polls.len() as i64 >= POLL_PAGE_SIZE;
    let pins = conversations_api::list_pinned(client, conversation_id)
        .unwrap_or_default()
        .into_iter()
        .map(|pin| pin.message_id)
        .collect();
    if let Some(up_to) = newest_timestamp {
        let _ = conversations_api::mark_read(client, conversation_id, Some(up_to));
    }

    Ok(LoadedConversation {
        detail,
        title,
        subtitle,
        peer_account_id,
        messages,
        polls,
        pins,
        history_cursor,
        history_has_older,
        poll_cursor,
        poll_has_older,
        poll_error,
    })
}

fn decrypt_direct_history(
    client: &friendshub_core::Client,
    rows: &[conversations_api::ConversationEnvelope],
    conversation_id: &str,
    account: &crate::platform::accounts::LocalAccount,
) -> Result<Vec<MessageEntry>, CoreError> {
    let mut messages = Vec::new();
    for row in rows {
        if row.recipient_device_number != account.device_number
            || row
                .conversation_id
                .as_deref()
                .is_some_and(|id| id != conversation_id)
        {
            continue;
        }
        if !matches!(
            row.envelope_type,
            ENVELOPE_TYPE_MESSAGE
                | ENVELOPE_TYPE_SYNC
                | ENVELOPE_TYPE_EDIT
                | ENVELOPE_TYPE_DELETE
                | ENVELOPE_TYPE_REACTION
        ) {
            continue;
        }
        let envelope = chat_api::HistoryEnvelope {
            envelope_id: row.envelope_id.clone(),
            sender_account_id: row.sender_account_id.clone(),
            sender_device_number: row.sender_device_number,
            recipient_device_number: row.recipient_device_number,
            envelope_type: row.envelope_type,
            is_prekey_message: row.is_prekey_message,
            ciphertext: row.ciphertext.clone(),
            client_timestamp: row.client_timestamp,
            server_timestamp: row.server_timestamp,
            conversation_id: row.conversation_id.clone(),
        };
        let decrypted = chat_api::decrypt_envelope(client, &envelope)?;
        let bytes = decode_hex(&decrypted.plaintext_hex).ok_or_else(|| {
            CoreError::InvalidPayload("decrypted history was not valid hex".into())
        })?;
        let payload_text = String::from_utf8(bytes)
            .map_err(|_| CoreError::InvalidPayload("decrypted history was not UTF-8".into()))?;
        let payload = match serde_json::from_str::<Value>(&payload_text) {
            Ok(value) => value,
            Err(_) if row.envelope_type == ENVELOPE_TYPE_MESSAGE => {
                add_plain_message(
                    &mut messages,
                    &row.envelope_id,
                    &row.sender_account_id,
                    &account.id,
                    &account.username,
                    &payload_text,
                    row.server_timestamp,
                );
                continue;
            }
            Err(_) => continue,
        };

        match row.envelope_type {
            ENVELOPE_TYPE_MESSAGE => {
                if payload.get("kind").and_then(Value::as_str) == Some("message") {
                    let Some(id) = payload.get("message_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(text) = payload.get("text").and_then(Value::as_str) else {
                        continue;
                    };
                    add_plain_message(
                        &mut messages,
                        id,
                        &row.sender_account_id,
                        &account.id,
                        &account.username,
                        text,
                        row.server_timestamp,
                    );
                }
            }
            ENVELOPE_TYPE_SYNC => {
                if let Some(legacy_hex) = payload.get("plaintext_hex").and_then(Value::as_str)
                    && let Some(legacy_bytes) = decode_hex(legacy_hex)
                    && let Ok(legacy_text) = String::from_utf8(legacy_bytes)
                {
                    let legacy_payload = serde_json::from_str::<Value>(&legacy_text).ok();
                    if let Some(legacy_payload) = legacy_payload
                        && legacy_payload.get("kind").and_then(Value::as_str) == Some("message")
                        && let (Some(id), Some(text)) = (
                            legacy_payload.get("message_id").and_then(Value::as_str),
                            legacy_payload.get("text").and_then(Value::as_str),
                        )
                    {
                        add_plain_message(
                            &mut messages,
                            id,
                            &row.sender_account_id,
                            &account.id,
                            &account.username,
                            text,
                            row.server_timestamp,
                        );
                    } else if !legacy_text.starts_with('{') {
                        add_plain_message(
                            &mut messages,
                            &row.envelope_id,
                            &row.sender_account_id,
                            &account.id,
                            &account.username,
                            &legacy_text,
                            row.server_timestamp,
                        );
                    }
                }
            }
            ENVELOPE_TYPE_EDIT => {
                if payload.get("kind").and_then(Value::as_str) == Some("edit")
                    && let (Some(target), Some(text)) = (
                        payload.get("target_message_id").and_then(Value::as_str),
                        payload.get("new_text").and_then(Value::as_str),
                    )
                    && let Some(message) = messages.iter_mut().find(|entry| entry.id == target)
                {
                    message.body = text.to_owned();
                    message.edited = true;
                }
            }
            ENVELOPE_TYPE_DELETE => {
                if payload.get("kind").and_then(Value::as_str) == Some("delete")
                    && let Some(target) = payload.get("target_message_id").and_then(Value::as_str)
                {
                    messages.retain(|entry| entry.id != target);
                }
            }
            ENVELOPE_TYPE_REACTION => {
                if payload.get("kind").and_then(Value::as_str) == Some("reaction")
                    && let Some(target) = payload.get("target_message_id").and_then(Value::as_str)
                    && let Some(message) = messages.iter_mut().find(|entry| entry.id == target)
                {
                    let emoji = payload.get("emoji").and_then(Value::as_str);
                    message
                        .reactions
                        .retain(|reaction| reaction.actor_id != row.sender_account_id);
                    if let Some(emoji) = emoji {
                        message.reactions.push(ReactionEntry {
                            actor_id: row.sender_account_id.clone(),
                            emoji: emoji.to_owned(),
                            mine: row.sender_account_id == account.id,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(messages)
}

fn add_plain_message(
    messages: &mut Vec<MessageEntry>,
    message_id: &str,
    sender_id: &str,
    account_id: &str,
    username: &str,
    text: &str,
    timestamp: i64,
) {
    if messages.iter().any(|message| message.id == message_id) {
        return;
    }
    messages.push(MessageEntry {
        id: message_id.to_owned(),
        day_label: utc_day_label(timestamp),
        author: if sender_id == account_id {
            username.into()
        } else {
            short_identifier(sender_id)
        },
        author_tag: String::new(),
        body: text.to_owned(),
        time_label: utc_time_label(timestamp),
        delivery_label: if sender_id == account_id {
            "Отправлено"
        } else {
            ""
        }
        .to_owned(),
        own: sender_id == account_id,
        edited: false,
        show_day_label: false,
        server_timestamp: timestamp,
        reactions: Vec::new(),
    });
}

fn load_poll_page(
    client: &friendshub_core::Client,
    conversation_id: &str,
    account: &crate::platform::accounts::LocalAccount,
    before: Option<i64>,
    before_id: Option<String>,
) -> Result<Vec<PollState>, CoreError> {
    let mut items = chat_api::list_conversation_polls(
        client,
        &chat_api::PollHistoryRequest {
            conversation_id: conversation_id.to_owned(),
            before,
            before_id,
            limit: Some(POLL_PAGE_SIZE),
        },
    )?;
    items.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let mut metadata = None;
        if let (Some(sender_account_id), Some(sender_device_number)) = (
            item.creator_account_id.as_deref(),
            item.creator_device_number,
        ) {
            if let Ok(value) = chat_api::decrypt_poll_payload(
                client,
                &chat_api::DecryptPollPayloadRequest {
                    sender_account_id: sender_account_id.to_owned(),
                    sender_device_number,
                    encrypted_meta: item.encrypted_meta.clone(),
                },
            ) {
                metadata = serde_json::from_value::<PollMetadataView>(value).ok();
            }
        }
        let mut extra_texts = HashMap::new();
        let mut extra_options = item.extra_options.iter().collect::<Vec<_>>();
        extra_options.sort_by_key(|option| option.created_at);
        for option in extra_options {
            if let (Some(sender_account_id), Some(sender_device_number)) = (
                option.proposed_by_account_id.as_deref(),
                option.proposed_by_device_number,
            ) {
                if let Ok(value) = chat_api::decrypt_poll_payload(
                    client,
                    &chat_api::DecryptPollPayloadRequest {
                        sender_account_id: sender_account_id.to_owned(),
                        sender_device_number,
                        encrypted_meta: option.encrypted_meta.clone(),
                    },
                ) {
                    if let Some(text) = value.get("text").and_then(Value::as_str) {
                        extra_texts.insert(option.option_id.clone(), text.to_owned());
                    }
                }
            }
        }
        let selected = item.my_votes.clone();
        result.push(PollState {
            owner: item.creator_account_id.as_deref() == Some(account.id.as_str()),
            item,
            metadata,
            extra_texts,
            selected,
            proposal: String::new(),
            error: String::new(),
            busy: false,
        });
    }
    Ok(result)
}

fn send_text(ctx: &AppContext, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let Some(session) = ctx.active_session() else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let editing_message_id = state_lock()
        .accounts
        .get(&session.account.id)
        .and_then(|account| account.chats.get(&conversation_id))
        .and_then(|chat| chat.editing_message_id.clone());
    if let Some(message_id) = editing_message_id {
        edit_message_text(ctx, &conversation_id, &message_id, text);
        return;
    }
    let id = uuid_v7();
    let request_key = format!("send:{id}");
    let request_key_for_error = request_key.clone();
    let conversation_id_for_error = conversation_id.clone();
    let request_generation = next_request_generation();
    {
        let mut state = state_lock();
        let chat = state
            .accounts
            .entry(session.account.id.clone())
            .or_default()
            .chats
            .entry(conversation_id.clone())
            .or_default();
        if chat.busy || chat.loading {
            return;
        }
        chat.busy = true;
        chat.composer_error.clear();
        chat.request_generations
            .insert(request_key.clone(), request_generation);
    }
    apply_selected_chat(ctx);
    let body = text.to_owned();
    let body_for_ui = body.clone();
    let id_for_ui = id.clone();
    let created_at = now_unix();
    let conversation_id_for_ui = conversation_id.clone();
    let account = session.account.clone();
    let account_id = account.id.clone();
    let account_id_for_ui = account_id.clone();
    let username = account.username.clone();
    let context = ctx.clone();
    let session_for_completion = session.clone();
    let session_for_error = session.clone();
    let conversation_id_for_state = conversation_id_for_ui.clone();
    let result = ctx.call(
        "friendshub-message-send",
        move |client| {
            let detail = conversations_api::get(client, conversation_id.clone())?;
            let draft = json!({
                "text": body,
                "replyTo": null,
                "forwardFrom": null,
                "attachmentIds": [],
            });
            let payload = friendshub_core::message_payload::build_message_payload(
                &id,
                &draft,
                created_at as f64,
            )
            .map_err(CoreError::InvalidPayload)?;
            let plaintext_hex = encode_hex(&serde_json::to_vec(&payload).map_err(CoreError::from)?);
            match detail.kind.as_str() {
                "direct" => {
                    let recipient = detail
                        .members
                        .iter()
                        .find(|member| member.as_str() != account.id)
                        .cloned()
                        .ok_or_else(|| {
                            CoreError::InvalidPayload("direct conversation has no peer".into())
                        })?;
                    let sent = chat_api::send_message(
                        client,
                        &chat_api::SendMessageRequest {
                            recipient_account_id: recipient,
                            plaintext_hex,
                            device_number: None,
                            conversation_id: Some(conversation_id.clone()),
                            refresh_devices: false,
                            sync: true,
                        },
                    )?;
                    if sent.envelopes.is_empty() {
                        return Err(CoreError::InvalidPayload(
                            "message was not delivered to a peer device".into(),
                        ));
                    }
                }
                "group" => {
                    let sent = chat_api::send_group_message(
                        client,
                        &chat_api::SendGroupMessageRequest {
                            conversation_id: conversation_id.clone(),
                            plaintext_hex,
                            sync: true,
                        },
                    )?;
                    if sent.envelopes.is_empty() {
                        return Err(CoreError::InvalidPayload(
                            "message was not delivered to a group member".into(),
                        ));
                    }
                }
                _ => {
                    return Err(CoreError::InvalidPayload(
                        "this conversation does not accept chat messages".into(),
                    ));
                }
            }
            Ok(())
        },
        move |window, result| match result {
            Ok(()) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id_for_ui,
                        &conversation_id_for_state,
                        &request_key,
                        request_generation,
                    )
                {
                    return;
                }
                let entry = MessageEntry {
                    id: id_for_ui,
                    day_label: utc_day_label(created_at),
                    author: username,
                    author_tag: String::new(),
                    body: body_for_ui,
                    time_label: utc_time_label(created_at),
                    delivery_label: "Отправлено".to_owned(),
                    own: true,
                    edited: false,
                    show_day_label: false,
                    server_timestamp: created_at,
                    reactions: Vec::new(),
                };
                let mut state = state_lock();
                let visible =
                    is_chat_route_visible(&window, &conversation_id_for_state, false, None);
                let chat = state
                    .accounts
                    .entry(account_id_for_ui.clone())
                    .or_default()
                    .chats
                    .entry(conversation_id_for_state.clone())
                    .or_default();
                chat.messages.push(entry);
                normalize_day_markers(&mut chat.messages);
                chat.busy = false;
                chat.draft.clear();
                chat.composer_error.clear();
                if visible {
                    apply_chat_models(window, chat);
                }
                drop(state);
                apply_list_from_context(&context, &account_id_for_ui);
            }
            Err(error) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id_for_ui,
                        &conversation_id_for_state,
                        &request_key,
                        request_generation,
                    )
                {
                    return;
                }
                let mut state = state_lock();
                let visible =
                    is_chat_route_visible(&window, &conversation_id_for_state, false, None);
                if let Some(chat) = state
                    .accounts
                    .entry(account_id_for_ui)
                    .or_default()
                    .chats
                    .get_mut(&conversation_id_for_state)
                {
                    chat.busy = false;
                    chat.composer_error = error.user_message().to_owned();
                    if visible {
                        apply_chat_models(window, chat);
                    }
                }
            }
        },
    );
    if let Err(error) = result {
        if !same_active_session(ctx, &session_for_error)
            || !chat_request_generation_is_current(
                &account_id,
                &conversation_id_for_error,
                &request_key_for_error,
                request_generation,
            )
        {
            return;
        }
        if let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(&conversation_id_for_error))
        {
            chat.busy = false;
            chat.composer_error = error.to_string();
        }
        apply_selected_chat(ctx);
    }
}

fn edit_message_text(ctx: &AppContext, conversation_id: &str, message_id: &str, text: &str) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let account_id = session.account.id.clone();
    let is_own_message = {
        let state = state_lock();
        state
            .accounts
            .get(&account_id)
            .and_then(|account| account.chats.get(conversation_id))
            .and_then(|chat| {
                chat.messages
                    .iter()
                    .find(|message| message.id == message_id)
            })
            .is_some_and(|message| message.own)
    };
    if !is_own_message || text.trim().is_empty() {
        return;
    }
    let request_key = format!("edit:{message_id}");
    let request_generation = next_request_generation();
    {
        let mut state = state_lock();
        let Some(chat) = state
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        else {
            return;
        };
        if chat.busy || chat.loading {
            return;
        }
        chat.busy = true;
        chat.composer_error.clear();
        chat.request_generations
            .insert(request_key.clone(), request_generation);
    }
    apply_selected_chat(ctx);

    let request = chat_api::EditMessageRequest {
        conversation_id: conversation_id.to_owned(),
        target_message_id: message_id.to_owned(),
        new_text: text.to_owned(),
    };
    let text_for_ui = text.to_owned();
    let account_id_for_ui = account_id.clone();
    let conversation_id_for_ui = conversation_id.to_owned();
    let message_id_for_ui = message_id.to_owned();
    let request_key_for_error = request_key.clone();
    let session_for_completion = session.clone();
    let session_for_error = session.clone();
    let context = ctx.clone();
    let result = ctx.call(
        "friendshub-message-edit",
        move |client| chat_api::edit_message(client, &request),
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !chat_request_generation_is_current(
                    &account_id_for_ui,
                    &conversation_id_for_ui,
                    &request_key,
                    request_generation,
                )
            {
                return;
            }
            let visible = is_chat_route_visible(&window, &conversation_id_for_ui, false, None);
            let mut state = state_lock();
            if let Some(chat) = state
                .accounts
                .entry(account_id_for_ui)
                .or_default()
                .chats
                .get_mut(&conversation_id_for_ui)
            {
                chat.busy = false;
                match result {
                    Ok(_) => {
                        if let Some(message) = chat
                            .messages
                            .iter_mut()
                            .find(|message| message.id == message_id_for_ui)
                        {
                            message.body = text_for_ui;
                            message.edited = true;
                        }
                        chat.editing = false;
                        chat.editing_message_id = None;
                        chat.draft.clear();
                        chat.composer_error.clear();
                        if !chat.search_query.trim().is_empty() {
                            update_chat_search(chat, &chat.search_query.clone());
                        }
                    }
                    Err(error) => chat.composer_error = error.user_message().to_owned(),
                }
                if visible {
                    apply_chat_models(window, chat);
                }
            }
        },
    );
    if let Err(error) = result {
        if !same_active_session(ctx, &session_for_error)
            || !chat_request_generation_is_current(
                &account_id,
                conversation_id,
                &request_key_for_error,
                request_generation,
            )
        {
            return;
        }
        if let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        {
            chat.busy = false;
            chat.composer_error = error.to_string();
        }
        apply_selected_chat(ctx);
    }
}

fn mark_conversation_read(ctx: &AppContext, conversation_id: &str, up_to: Option<i64>) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let completion_conversation_id = conversation_id.to_owned();
    let id = completion_conversation_id.clone();
    let account_id = session.account.id.clone();
    let request_key = format!("mark-read:{conversation_id}");
    let request_generation = begin_account_request(&account_id, &request_key);
    let session_for_completion = session.clone();
    let context = ctx.clone();
    let _ = ctx.call(
        "friendshub-conversation-mark-read",
        move |client| conversations_api::mark_read(client, id, up_to),
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !account_request_generation_is_current(
                    &account_id,
                    &request_key,
                    request_generation,
                )
                || !is_conversation_screen(&window)
            {
                return;
            }
            if let Err(error) = result {
                window.set_conversations_error(error.user_message().into());
            } else {
                if let Some(conversation) =
                    state_lock()
                        .accounts
                        .get_mut(&account_id)
                        .and_then(|account| {
                            account
                                .conversations
                                .iter_mut()
                                .find(|conversation| conversation.id == completion_conversation_id)
                        })
                {
                    conversation.unread_count = 0;
                }
                apply_list_from_context(&context, &account_id);
            }
        },
    );
}

fn toggle_archive(ctx: &AppContext, conversation_id: &str) {
    let archive = state_lock()
        .accounts
        .values()
        .flat_map(|account| account.conversations.iter())
        .find(|conversation| conversation.id == conversation_id)
        .is_none_or(|conversation| conversation.archived_at.is_none());
    let id = conversation_id.to_owned();
    mutate_conversation(
        ctx,
        conversation_id,
        "friendshub-conversation-archive",
        move |client| {
            if archive {
                conversations_api::archive(client, id)
            } else {
                conversations_api::unarchive(client, id)
            }
        },
    );
}

fn toggle_mute(ctx: &AppContext, conversation_id: &str) {
    let muted = state_lock()
        .accounts
        .values()
        .flat_map(|account| account.conversations.iter())
        .find(|conversation| conversation.id == conversation_id)
        .and_then(|conversation| conversation.muted_until)
        .is_some_and(|until| until > now_unix());
    let id = conversation_id.to_owned();
    mutate_conversation(
        ctx,
        conversation_id,
        "friendshub-conversation-mute",
        move |client| {
            if muted {
                conversations_api::unmute(client, id)
            } else {
                conversations_api::mute(client, id, None)
            }
        },
    );
}

fn leave_conversation(ctx: &AppContext, conversation_id: &str) {
    let id = conversation_id.to_owned();
    mutate_conversation(
        ctx,
        conversation_id,
        "friendshub-conversation-leave",
        move |client| conversations_api::leave(client, id),
    );
}

fn mutate_conversation(
    ctx: &AppContext,
    conversation_id: &str,
    name: &'static str,
    operation: impl FnOnce(&friendshub_core::Client) -> Result<(), CoreError> + Send + 'static,
) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let context = ctx.clone();
    let account_id = session.account.id.clone();
    let request_key = format!("{name}:{conversation_id}");
    let request_generation = begin_account_request(&account_id, &request_key);
    let session_for_completion = session;
    let context_for_completion = context.clone();
    let _ = ctx.call(name, operation, move |window, result| match result {
        result => {
            if !same_active_session(&context_for_completion, &session_for_completion)
                || !account_request_generation_is_current(
                    &account_id,
                    &request_key,
                    request_generation,
                )
                || !is_conversation_screen(&window)
            {
                return;
            }
            match result {
                Ok(()) => load(&context_for_completion, window.get_route().as_str()),
                Err(error) => window.set_conversations_error(error.user_message().into()),
            }
        }
    });
}

fn load_older_messages(ctx: &AppContext, conversation_id: &str) -> std::io::Result<()> {
    let Some(session) = ctx.active_session() else {
        return Ok(());
    };
    let account_id = session.account.id.clone();
    let request_generation = next_request_generation();
    let cursor = {
        let mut state = state_lock();
        let Some(chat) = state
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        else {
            return Ok(());
        };
        if !chat.history_has_older || chat.history_loading_older {
            return Ok(());
        }
        chat.history_loading_older = true;
        chat.request_generations
            .insert("history-older".to_owned(), request_generation);
        chat.history_cursor.clone()
    };
    let Some(mut cursor) = cursor else {
        if let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        {
            chat.history_loading_older = false;
        }
        return Ok(());
    };
    cursor.limit = Some(HISTORY_PAGE_SIZE);
    apply_selected_chat(ctx);
    let id = conversation_id.to_owned();
    let id_for_ui = id.clone();
    let context = ctx.clone();
    let session_for_completion = session.clone();
    let session_for_error = session.clone();
    let account = session.account;
    let account_id_for_ui = account_id.clone();
    let result = ctx.call(
        "friendshub-chat-history-older",
        move |client| {
            let rows = conversations_api::envelopes(client, id.clone(), cursor)?;
            let newest_cursor = rows.first().map(|row| conversations_api::HistoryCursor {
                before: Some(row.server_timestamp),
                before_id: Some(row.envelope_id.clone()),
                limit: Some(HISTORY_PAGE_SIZE),
            });
            let has_older = rows.len() as i64 >= HISTORY_PAGE_SIZE;
            let mut ordered = rows;
            ordered.sort_by(|left, right| {
                left.server_timestamp
                    .cmp(&right.server_timestamp)
                    .then_with(|| left.envelope_id.cmp(&right.envelope_id))
            });
            let messages = decrypt_direct_history(client, &ordered, &id, &account)?;
            Ok((messages, newest_cursor, has_older))
        },
        move |window, result| match result {
            Ok((older, cursor, has_older)) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id_for_ui,
                        &id_for_ui,
                        "history-older",
                        request_generation,
                    )
                {
                    return;
                }
                let visible = is_chat_route_visible(&window, &id_for_ui, false, None);
                let mut state = state_lock();
                if let Some(chat) = state
                    .accounts
                    .entry(account_id_for_ui.clone())
                    .or_default()
                    .chats
                    .get_mut(&id_for_ui)
                {
                    let existing = chat
                        .messages
                        .iter()
                        .map(|entry| entry.id.clone())
                        .collect::<HashSet<_>>();
                    chat.messages.extend(
                        older
                            .into_iter()
                            .filter(|entry| !existing.contains(&entry.id)),
                    );
                    chat.messages.sort_by_key(|entry| entry.server_timestamp);
                    normalize_day_markers(&mut chat.messages);
                    chat.history_cursor = cursor;
                    chat.history_has_older = has_older;
                    chat.history_loading_older = false;
                    if visible {
                        apply_chat_models(window, chat);
                    }
                }
            }
            Err(error) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id_for_ui,
                        &id_for_ui,
                        "history-older",
                        request_generation,
                    )
                {
                    return;
                }
                let visible = is_chat_route_visible(&window, &id_for_ui, false, None);
                if let Some(chat) = state_lock()
                    .accounts
                    .entry(account_id_for_ui)
                    .or_default()
                    .chats
                    .get_mut(&id_for_ui)
                {
                    chat.history_loading_older = false;
                    chat.error = error.user_message().to_owned();
                    if visible {
                        apply_chat_models(window, chat);
                    }
                }
            }
        },
    );
    if let Err(error) = result {
        if !same_active_session(ctx, &session_for_error)
            || !chat_request_generation_is_current(
                &account_id,
                conversation_id,
                "history-older",
                request_generation,
            )
        {
            return Ok(());
        }
        if let Some(chat) = state_lock()
            .accounts
            .entry(account_id)
            .or_default()
            .chats
            .get_mut(conversation_id)
        {
            chat.history_loading_older = false;
            chat.error = error.to_string();
        }
        apply_selected_chat(ctx);
        return Err(error);
    }
    Ok(())
}

fn set_reaction(ctx: &AppContext, index: usize, emoji: &str) {
    if emoji.is_empty() {
        return;
    }
    let Some(session) = ctx.active_session() else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let message_id = {
        let state = state_lock();
        state
            .accounts
            .get(&session.account.id)
            .and_then(|account| account.chats.get(&conversation_id))
            .and_then(|chat| chat.messages.get(index))
            .map(|message| message.id.clone())
    };
    let Some(message_id) = message_id else {
        return;
    };
    let remove = {
        let state = state_lock();
        state
            .accounts
            .get(&session.account.id)
            .and_then(|account| account.chats.get(&conversation_id))
            .and_then(|chat| chat.messages.get(index))
            .and_then(|message| {
                message
                    .reactions
                    .iter()
                    .find(|reaction| reaction.actor_id == session.account.id)
            })
            .is_some_and(|reaction| reaction.emoji == emoji)
    };
    let account_id = session.account.id.clone();
    let own_account_id = session.account.id.clone();
    let conversation_id_for_ui = conversation_id.clone();
    let message_id_for_ui = message_id.clone();
    let request_key = format!("reaction:{message_id}");
    let request_generation = begin_chat_request(&account_id, &conversation_id, &request_key);
    let emoji_owned = emoji.to_owned();
    let emoji_for_ui = emoji_owned.clone();
    let session_for_completion = session.clone();
    let context = ctx.clone();
    let _ = ctx.call(
        "friendshub-message-reaction",
        move |client| {
            chat_api::set_message_reaction(
                client,
                &chat_api::SetMessageReactionRequest {
                    conversation_id,
                    target_message_id: message_id.clone(),
                    emoji: if remove {
                        None
                    } else {
                        Some(emoji_owned.clone())
                    },
                },
            )
        },
        move |window, result| match result {
            Ok(_) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id,
                        &conversation_id_for_ui,
                        &request_key,
                        request_generation,
                    )
                {
                    return;
                }
                let visible = is_chat_route_visible(&window, &conversation_id_for_ui, false, None);
                let mut state = state_lock();
                if let Some(chat) = state
                    .accounts
                    .entry(account_id)
                    .or_default()
                    .chats
                    .get_mut(&conversation_id_for_ui)
                {
                    if let Some(message) = chat
                        .messages
                        .iter_mut()
                        .find(|message| message.id == message_id_for_ui)
                    {
                        message
                            .reactions
                            .retain(|reaction| reaction.actor_id != own_account_id);
                        if !remove {
                            message.reactions.push(ReactionEntry {
                                actor_id: own_account_id,
                                emoji: emoji_for_ui,
                                mine: true,
                            });
                        }
                    }
                    if visible {
                        apply_chat_models(window, chat);
                    }
                }
            }
            Err(error) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id,
                        &conversation_id_for_ui,
                        &request_key,
                        request_generation,
                    )
                {
                    return;
                }
                let visible = is_chat_route_visible(&window, &conversation_id_for_ui, false, None);
                let message = error.user_message().to_owned();
                if let Some(chat) = state_lock()
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conversation_id_for_ui))
                {
                    chat.error = message;
                    if visible {
                        apply_chat_models(window, chat);
                    }
                }
            }
        },
    );
}

fn move_pin_selection(ctx: &AppContext, action: &str) {
    let Some(account_id) = active_account_id(ctx) else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let mut state = state_lock();
    if let Some(chat) = state
        .accounts
        .get_mut(&account_id)
        .and_then(|account| account.chats.get_mut(&conversation_id))
    {
        if chat.pins.is_empty() {
            return;
        }
        match action {
            "pins-open" => chat.selected_pin = 0,
            "pins-previous" => {
                chat.selected_pin = if chat.selected_pin == 0 {
                    chat.pins.len() - 1
                } else {
                    chat.selected_pin - 1
                }
            }
            "pins-next" => chat.selected_pin = (chat.selected_pin + 1) % chat.pins.len(),
            _ => {}
        }
    }
    drop(state);
    apply_selected_chat(ctx);
}

fn unpin_current(ctx: &AppContext) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let account_id = session.account.id.clone();
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let message_id = {
        let state = state_lock();
        state
            .accounts
            .get(&account_id)
            .and_then(|account| account.chats.get(&conversation_id))
            .and_then(|chat| chat.pins.get(chat.selected_pin))
            .cloned()
    };
    let Some(message_id) = message_id else {
        return;
    };
    let message_id_for_ui = message_id.clone();
    let request_key = format!("unpin:{message_id}");
    let request_generation = begin_chat_request(&account_id, &conversation_id, &request_key);
    let conv_id = conversation_id.clone();
    let session_for_completion = session;
    let context = ctx.clone();
    let _ = ctx.call(
        "friendshub-conversation-unpin",
        move |client| conversations_api::unpin(client, conv_id, message_id.clone()),
        move |window, result| match result {
            Ok(()) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id,
                        &conversation_id,
                        &request_key,
                        request_generation,
                    )
                {
                    return;
                }
                let visible = is_chat_route_visible(&window, &conversation_id, false, None);
                let mut state = state_lock();
                if let Some(chat) = state
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conversation_id))
                {
                    chat.pins.retain(|id| id != &message_id_for_ui);
                    chat.selected_pin = 0;
                    if visible {
                        apply_chat_models(window, chat);
                    }
                }
            }
            Err(error) => {
                if !same_active_session(&context, &session_for_completion)
                    || !chat_request_generation_is_current(
                        &account_id,
                        &conversation_id,
                        &request_key,
                        request_generation,
                    )
                {
                    return;
                }
                let visible = is_chat_route_visible(&window, &conversation_id, false, None);
                let message = error.user_message().to_owned();
                if let Some(chat) = state_lock()
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conversation_id))
                {
                    chat.error = message;
                    if visible {
                        apply_chat_models(window, chat);
                    }
                }
            }
        },
    );
}

fn select_poll_option(ctx: &AppContext, poll_id: &str, option_id: &str) {
    let Some(account_id) = active_account_id(ctx) else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let mut state = state_lock();
    if let Some(chat) = state
        .accounts
        .get_mut(&account_id)
        .and_then(|account| account.chats.get_mut(&conversation_id))
        && let Some(poll) = chat.polls.iter_mut().find(|poll| poll.item.id == poll_id)
    {
        if !poll.item.allows_multiple {
            poll.selected.clear();
            poll.selected.push(option_id.to_owned());
        } else if let Some(index) = poll.selected.iter().position(|id| id == option_id) {
            poll.selected.remove(index);
        } else {
            poll.selected.push(option_id.to_owned());
        }
        let selections = poll.selected.clone();
        chat.poll_selections.insert(poll_id.to_owned(), selections);
        if let Some(window) = ctx.ui.upgrade() {
            apply_chat_models(window, chat);
        }
    }
}

fn vote_poll(ctx: &AppContext, poll_id: &str) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let selected = {
        let mut state = state_lock();
        let Some(chat) = state
            .accounts
            .entry(session.account.id.clone())
            .or_default()
            .chats
            .get_mut(&conversation_id)
        else {
            return;
        };
        let Some(poll) = chat.polls.iter_mut().find(|poll| poll.item.id == poll_id) else {
            return;
        };
        poll.busy = true;
        let selected = poll.selected.clone();
        if let Some(window) = ctx.ui.upgrade() {
            apply_chat_models(window, chat);
        }
        selected
    };
    let id = poll_id.to_owned();
    let account_id = session.account.id.clone();
    let request_key = format!("poll-vote:{poll_id}");
    let request_generation = begin_chat_request(&account_id, &conversation_id, &request_key);
    let request_key_for_error = request_key.clone();
    let account_id_for_error = account_id.clone();
    let poll_id_for_ui = id.clone();
    let conv_for_ui = conversation_id.clone();
    let session_for_completion = session.clone();
    let context = ctx.clone();
    let result = ctx.call(
        "friendshub-poll-vote",
        move |client| {
            chat_api::vote_poll(
                client,
                &chat_api::PollVoteRequest {
                    poll_id: id,
                    option_ids: selected,
                },
            )
        },
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !chat_request_generation_is_current(
                    &account_id,
                    &conv_for_ui,
                    &request_key,
                    request_generation,
                )
            {
                return;
            }
            let visible = is_chat_route_visible(&window, &conv_for_ui, false, None);
            let mut state = state_lock();
            if let Some(chat) = state
                .accounts
                .entry(account_id.clone())
                .or_default()
                .chats
                .get_mut(&conv_for_ui)
            {
                if let Some(poll) = chat
                    .polls
                    .iter_mut()
                    .find(|poll| poll.item.id == poll_id_for_ui)
                {
                    poll.busy = false;
                    match result {
                        Ok(item) => {
                            poll.selected = item.my_votes.clone();
                            poll.item = item;
                            poll.error.clear();
                        }
                        Err(error) => poll.error = error.user_message().to_owned(),
                    }
                }
                if visible {
                    apply_chat_models(window, chat);
                }
            }
            drop(state);
        },
    );
    if result.is_err()
        && same_active_session(ctx, &session)
        && chat_request_generation_is_current(
            &account_id_for_error,
            &conversation_id,
            &request_key_for_error,
            request_generation,
        )
    {
        if let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id_for_error)
            .and_then(|account| account.chats.get_mut(&conversation_id))
        {
            if let Some(poll) = chat.polls.iter_mut().find(|poll| poll.item.id == poll_id) {
                poll.busy = false;
            }
        }
        apply_selected_chat(ctx);
    }
}

fn add_poll_option(ctx: &AppContext, poll_id: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let Some(session) = ctx.active_session() else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let option_id = uuid_v7().replace('-', "");
    let id = poll_id.to_owned();
    let poll_id_for_ui = id.clone();
    let option_text = text.to_owned();
    let text_for_ui = option_text.clone();
    let option_id_for_ui = option_id.clone();
    let conv_id = conversation_id.clone();
    let account_id = session.account.id.clone();
    let owner_id = session.account.id.clone();
    let owner_device = session.account.device_number;
    let conv_for_ui = conversation_id.clone();
    let request_key = format!("poll-add-option:{poll_id}:{option_id}");
    let request_generation = begin_chat_request(&account_id, &conversation_id, &request_key);
    let session_for_completion = session.clone();
    let context = ctx.clone();
    let _ = ctx.call(
        "friendshub-poll-add-option",
        move |client| {
            let encrypted_meta = chat_api::encrypt_poll_option(
                client,
                &chat_api::EncryptPollOptionRequest {
                    conversation_id: conv_id,
                    text: option_text.clone(),
                },
            )?;
            chat_api::add_poll_option(
                client,
                &chat_api::AddPollOptionRequest {
                    poll_id: id.clone(),
                    option_id: option_id.clone(),
                    encrypted_meta,
                },
            )?;
            Ok(())
        },
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !chat_request_generation_is_current(
                    &account_id,
                    &conv_for_ui,
                    &request_key,
                    request_generation,
                )
            {
                return;
            }
            let visible = is_chat_route_visible(&window, &conv_for_ui, false, None);
            let mut state = state_lock();
            if let Some(chat) = state
                .accounts
                .entry(account_id)
                .or_default()
                .chats
                .get_mut(&conv_for_ui)
            {
                if let Some(poll) = chat
                    .polls
                    .iter_mut()
                    .find(|poll| poll.item.id == poll_id_for_ui)
                {
                    match result {
                        Ok(()) => {
                            poll.extra_texts
                                .insert(option_id_for_ui.clone(), text_for_ui);
                            poll.item.extra_options.push(chat_api::PollExtraOption {
                                option_id: option_id_for_ui.clone(),
                                encrypted_meta: String::new(),
                                proposed_by_account_id: Some(owner_id),
                                proposed_by_device_number: Some(owner_device),
                                created_at: now_unix(),
                            });
                            poll.item.counts.push(chat_api::PollCount {
                                option_id: option_id_for_ui,
                                count: 0,
                            });
                            poll.error.clear();
                        }
                        Err(error) => poll.error = error.user_message().to_owned(),
                    }
                }
                if visible {
                    apply_chat_models(window, chat);
                }
            }
        },
    );
}

enum PollMutation {
    Stop,
    Delete,
}

fn mutate_poll(ctx: &AppContext, poll_id: &str, mutation: PollMutation) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let id = poll_id.to_owned();
    let poll_id_for_ui = id.clone();
    let operation = match mutation {
        PollMutation::Stop => 0,
        PollMutation::Delete => 1,
    };
    let account_id = session.account.id.clone();
    let conv_id = conversation_id.clone();
    let request_key = format!("poll-mutate:{poll_id}");
    let request_generation = begin_chat_request(&account_id, &conversation_id, &request_key);
    let session_for_completion = session.clone();
    let context = ctx.clone();
    let _ = ctx.call(
        "friendshub-poll-mutate",
        move |client| {
            let request = chat_api::PollIdRequest {
                poll_id: id.clone(),
            };
            if operation == 0 {
                chat_api::stop_poll(client, &request)
            } else {
                chat_api::delete_poll(client, &request)
            }
        },
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !chat_request_generation_is_current(
                    &account_id,
                    &conv_id,
                    &request_key,
                    request_generation,
                )
            {
                return;
            }
            let visible = is_chat_route_visible(&window, &conv_id, false, None);
            if let Err(error) = result {
                let message = error.user_message().to_owned();
                if let Some(chat) = state_lock()
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conv_id))
                {
                    chat.polls_error = message;
                    if visible {
                        apply_chat_models(window, chat);
                    }
                }
                return;
            }
            if operation == 1 {
                if let Some(chat) = state_lock()
                    .accounts
                    .get_mut(&account_id)
                    .and_then(|account| account.chats.get_mut(&conv_id))
                {
                    chat.polls_error.clear();
                    chat.polls.retain(|poll| poll.item.id != poll_id_for_ui);
                    if visible {
                        apply_chat_models(window, chat);
                    }
                }
            } else if let Some(chat) = state_lock()
                .accounts
                .get_mut(&account_id)
                .and_then(|account| account.chats.get_mut(&conv_id))
            {
                chat.polls_error.clear();
                if let Some(poll) = chat
                    .polls
                    .iter_mut()
                    .find(|poll| poll.item.id == poll_id_for_ui)
                {
                    poll.item.closed_at = Some(now_unix());
                    poll.item.revision += 1;
                }
                if visible {
                    apply_chat_models(window, chat);
                }
            }
        },
    );
}

fn remove_poll_option(ctx: &AppContext, poll_id: &str, option_id: &str) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let account_id = session.account.id.clone();
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let request = chat_api::PollOptionMutationRequest {
        poll_id: poll_id.to_owned(),
        option_id: option_id.to_owned(),
    };
    let conv = conversation_id.clone();
    let poll = poll_id.to_owned();
    let option = option_id.to_owned();
    let poll_for_ui = poll.clone();
    let option_for_ui = option.clone();
    let request_key = format!("poll-remove-option:{poll_id}:{option_id}");
    let request_generation = begin_chat_request(&account_id, &conversation_id, &request_key);
    let session_for_completion = session;
    let context = ctx.clone();
    let _ = ctx.call(
        "friendshub-poll-remove-option",
        move |client| chat_api::delete_poll_option(client, &request),
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !chat_request_generation_is_current(
                    &account_id,
                    &conv,
                    &request_key,
                    request_generation,
                )
            {
                return;
            }
            let visible = is_chat_route_visible(&window, &conv, false, None);
            let mut state = state_lock();
            if let Some(chat) = state
                .accounts
                .entry(account_id)
                .or_default()
                .chats
                .get_mut(&conv)
            {
                if let Some(poll) = chat
                    .polls
                    .iter_mut()
                    .find(|poll| poll.item.id == poll_for_ui)
                {
                    match result {
                        Ok(()) => {
                            if !poll.item.deleted_option_ids.contains(&option_for_ui) {
                                poll.item.deleted_option_ids.push(option_for_ui.clone());
                            }
                            poll.selected.retain(|id| id != &option_for_ui);
                            poll.error.clear();
                        }
                        Err(error) => poll.error = error.user_message().to_owned(),
                    }
                }
                if visible {
                    apply_chat_models(window, chat);
                }
            }
        },
    );
}

fn reload_polls(ctx: &AppContext, conversation_id: &str) -> std::io::Result<()> {
    let Some(session) = ctx.active_session() else {
        return Ok(());
    };
    let account_id = session.account.id.clone();
    let request_generation = next_request_generation();
    {
        let mut state = state_lock();
        let Some(chat) = state
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        else {
            return Ok(());
        };
        if chat.polls_loading {
            return Ok(());
        }
        chat.polls_loading = true;
        chat.polls_error.clear();
        chat.request_generations
            .insert("polls-reload".to_owned(), request_generation);
    }
    apply_selected_chat(ctx);
    let conv = conversation_id.to_owned();
    let conv_for_ui = conv.clone();
    let account_id_for_ui = account_id.clone();
    let session_for_completion = session.clone();
    let session_for_error = session.clone();
    let account = session.account;
    let context = ctx.clone();
    let result = ctx.call(
        "friendshub-polls-reload",
        move |client| load_poll_page(client, &conv, &account, None, None),
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !chat_request_generation_is_current(
                    &account_id_for_ui,
                    &conv_for_ui,
                    "polls-reload",
                    request_generation,
                )
            {
                return;
            }
            let visible = is_chat_route_visible(&window, &conv_for_ui, false, None);
            let mut state = state_lock();
            if let Some(chat) = state
                .accounts
                .entry(account_id_for_ui)
                .or_default()
                .chats
                .get_mut(&conv_for_ui)
            {
                chat.polls_loading = false;
                match result {
                    Ok(polls) => {
                        chat.poll_cursor = polls.first().map(|poll| chat_api::PollHistoryRequest {
                            conversation_id: conv_for_ui.clone(),
                            before: Some(poll.item.created_at),
                            before_id: Some(poll.item.id.clone()),
                            limit: Some(POLL_PAGE_SIZE),
                        });
                        chat.poll_has_older = polls.len() as i64 >= POLL_PAGE_SIZE;
                        chat.polls_error.clear();
                        merge_poll_pages(&mut chat.polls, polls);
                    }
                    Err(error) => chat.polls_error = error.user_message().to_owned(),
                }
                if visible {
                    apply_chat_models(window, chat);
                }
            }
        },
    );
    if let Err(error) = result {
        if !same_active_session(ctx, &session_for_error)
            || !chat_request_generation_is_current(
                &account_id,
                conversation_id,
                "polls-reload",
                request_generation,
            )
        {
            return Err(error);
        }
        if let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        {
            chat.polls_loading = false;
            chat.polls_error = error.to_string();
        }
        apply_selected_chat(ctx);
        return Err(error);
    }
    Ok(())
}

fn load_older_polls(ctx: &AppContext, conversation_id: &str) -> std::io::Result<()> {
    let Some(session) = ctx.active_session() else {
        return Ok(());
    };
    let account_id = session.account.id.clone();
    let request_generation = next_request_generation();
    let cursor = {
        let mut state = state_lock();
        let Some(chat) = state
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        else {
            return Ok(());
        };
        if !chat.poll_has_older || chat.polls_loading_older {
            return Ok(());
        }
        let Some(cursor) = chat.poll_cursor.clone() else {
            return Ok(());
        };
        chat.polls_loading_older = true;
        chat.request_generations
            .insert("polls-older".to_owned(), request_generation);
        cursor
    };
    apply_selected_chat(ctx);
    let conv = conversation_id.to_owned();
    let conv_for_ui = conv.clone();
    let account_id_for_ui = account_id.clone();
    let session_for_completion = session.clone();
    let session_for_error = session.clone();
    let account = session.account;
    let context = ctx.clone();
    let result = ctx.call(
        "friendshub-polls-older",
        move |client| {
            let polls = load_poll_page(client, &conv, &account, cursor.before, cursor.before_id)?;
            let next = polls.first().map(|poll| chat_api::PollHistoryRequest {
                conversation_id: conv.clone(),
                before: Some(poll.item.created_at),
                before_id: Some(poll.item.id.clone()),
                limit: Some(POLL_PAGE_SIZE),
            });
            let has_older = polls.len() as i64 >= POLL_PAGE_SIZE;
            Ok((polls, next, has_older))
        },
        move |window, result| {
            if !same_active_session(&context, &session_for_completion)
                || !chat_request_generation_is_current(
                    &account_id_for_ui,
                    &conv_for_ui,
                    "polls-older",
                    request_generation,
                )
            {
                return;
            }
            let visible = is_chat_route_visible(&window, &conv_for_ui, false, None);
            let mut state = state_lock();
            if let Some(chat) = state
                .accounts
                .entry(account_id_for_ui)
                .or_default()
                .chats
                .get_mut(&conv_for_ui)
            {
                chat.polls_loading_older = false;
                match result {
                    Ok((polls, cursor, has_older)) => {
                        chat.poll_cursor = cursor;
                        chat.poll_has_older = has_older;
                        chat.polls_error.clear();
                        merge_poll_pages(&mut chat.polls, polls);
                    }
                    Err(error) => chat.polls_error = error.user_message().to_owned(),
                }
                if visible {
                    apply_chat_models(window, chat);
                }
            }
        },
    );
    if let Err(error) = result {
        if !same_active_session(ctx, &session_for_error)
            || !chat_request_generation_is_current(
                &account_id,
                conversation_id,
                "polls-older",
                request_generation,
            )
        {
            return Err(error);
        }
        if let Some(chat) = state_lock()
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(conversation_id))
        {
            chat.polls_loading_older = false;
            chat.polls_error = error.to_string();
        }
        apply_selected_chat(ctx);
        return Err(error);
    }
    Ok(())
}

fn merge_poll_pages(existing: &mut Vec<PollState>, incoming: Vec<PollState>) {
    for mut incoming_poll in incoming {
        if let Some(current) = existing
            .iter_mut()
            .find(|poll| poll.item.id == incoming_poll.item.id)
        {
            current.item = incoming_poll.item;
            if current.metadata.is_none() {
                current.metadata = incoming_poll.metadata.take();
            }
            current.extra_texts.extend(incoming_poll.extra_texts);
            current.selected = current.item.my_votes.clone();
        } else {
            existing.push(incoming_poll);
        }
    }
    existing.sort_by(|left, right| {
        left.item
            .created_at
            .cmp(&right.item.created_at)
            .then_with(|| left.item.id.cmp(&right.item.id))
    });
}

fn apply_conversation_list(ctx: &AppContext, window: MainWindow, account_id: &str) {
    let (conversations, folders, filter, folder_id, search, chats) = {
        let state = state_lock();
        let account = state.accounts.get(account_id);
        (
            account
                .map(|account| account.conversations.clone())
                .unwrap_or_default(),
            account
                .map(|account| account.folders.clone())
                .unwrap_or_default(),
            account
                .map(|account| account.selected_filter.clone())
                .unwrap_or_default(),
            account
                .map(|account| account.selected_folder.clone())
                .unwrap_or_default(),
            account
                .map(|account| account.search.clone())
                .unwrap_or_default(),
            account
                .map(|account| {
                    account
                        .chats
                        .iter()
                        .map(|(id, chat)| {
                            (
                                id.clone(),
                                (
                                    chat.title.clone(),
                                    chat.peer_account_id.clone(),
                                    chat.pins.len(),
                                    chat.messages.last().map(|message| message.body.clone()),
                                ),
                            )
                        })
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default(),
        )
    };
    let filtered = conversations
        .iter()
        .filter(|conversation| {
            if !folder_id.is_empty() {
                return folders
                    .iter()
                    .find(|folder| folder.id == folder_id)
                    .is_some_and(|folder| folder.conversation_ids.contains(&conversation.id));
            }
            match filter.as_str() {
                "personal" => conversation.kind == "direct",
                "groups" => conversation.kind == "group",
                "channels" => conversation.kind == "channel",
                "unread" => conversation.archived_at.is_none() && conversation.unread_count > 0,
                "archived" => conversation.archived_at.is_some(),
                _ => conversation.archived_at.is_none(),
            }
        })
        .filter(|conversation| {
            search.is_empty()
                || row_title(conversation, chats.get(&conversation.id))
                    .to_lowercase()
                    .contains(&search.to_lowercase())
        })
        .map(|conversation| {
            let meta = chats.get(&conversation.id);
            let title = row_title(conversation, meta);
            ConversationRow {
                id: conversation.id.clone().into(),
                title: title.clone().into(),
                avatar_label: initials(&title).into(),
                avatar_image: Image::default(),
                has_avatar: false,
                peer_account_id: meta
                    .map(|(_, peer, _, _)| peer.clone())
                    .unwrap_or_default()
                    .into(),
                preview: meta
                    .and_then(|(_, _, _, preview)| preview.clone())
                    .unwrap_or_default()
                    .into(),
                timestamp: conversation
                    .last_envelope_at
                    .map(utc_time_label)
                    .unwrap_or_default()
                    .into(),
                unread_count: conversation.unread_count.clamp(0, i64::from(i32::MAX)) as i32,
                muted: conversation
                    .muted_until
                    .is_some_and(|until| until > now_unix()),
                has_pinned: meta.is_some_and(|(_, _, pin_count, _)| *pin_count > 0),
                archived: conversation.archived_at.is_some(),
            }
        })
        .collect::<Vec<_>>();

    let filter_tabs = vec![
        FilterTab {
            id: "all".into(),
            label: localized("Все", "All").into(),
            unread_count: unread_count_for_kind(&conversations, None),
            visible: true,
        },
        FilterTab {
            id: "unread".into(),
            label: localized("Непрочитанные", "Unread").into(),
            unread_count: unread_count_for_kind(&conversations, None),
            visible: true,
        },
        FilterTab {
            id: "personal".into(),
            label: localized("Личные", "Personal").into(),
            unread_count: unread_count_for_kind(&conversations, Some("direct")),
            visible: true,
        },
        FilterTab {
            id: "groups".into(),
            label: localized("Группы", "Groups").into(),
            unread_count: unread_count_for_kind(&conversations, Some("group")),
            visible: true,
        },
        FilterTab {
            id: "channels".into(),
            label: localized("Каналы", "Channels").into(),
            unread_count: unread_count_for_kind(&conversations, Some("channel")),
            visible: true,
        },
    ];
    let folder_tabs = folders
        .iter()
        .map(|folder| FolderTab {
            id: folder.id.clone().into(),
            name: folder.name.clone().into(),
            unread_count: conversations
                .iter()
                .filter(|conversation| {
                    conversation.archived_at.is_none()
                        && conversation.unread_count > 0
                        && folder.conversation_ids.contains(&conversation.id)
                })
                .fold(0_i64, |total, conversation| {
                    total.saturating_add(conversation.unread_count)
                })
                .clamp(0, i64::from(i32::MAX)) as i32,
        })
        .collect::<Vec<_>>();
    window.set_conversations(ModelRc::new(VecModel::from(filtered)));
    window.set_conversation_filters(ModelRc::new(VecModel::from(filter_tabs)));
    window.set_folders(ModelRc::new(VecModel::from(folder_tabs)));
    window.set_selected_filter(if filter.is_empty() { "all" } else { &filter }.into());
    window.set_selected_folder(folder_id.into());
    window.set_conversation_search(search.into());
    window.set_has_archived(
        conversations
            .iter()
            .any(|conversation| conversation.archived_at.is_some()),
    );
    let _ = ctx;
}

fn unread_count_for_kind(
    conversations: &[conversations_api::Conversation],
    kind: Option<&str>,
) -> i32 {
    conversations
        .iter()
        .filter(|conversation| {
            conversation.archived_at.is_none()
                && conversation.unread_count > 0
                && kind.map_or(true, |kind| conversation.kind == kind)
        })
        .fold(0_i64, |total, conversation| {
            total.saturating_add(conversation.unread_count)
        })
        .clamp(0, i64::from(i32::MAX)) as i32
}

fn apply_chat_models(window: MainWindow, chat: &mut ChatState) {
    normalize_day_markers(&mut chat.messages);
    window.set_messages(ModelRc::new(VecModel::from(
        chat.messages.iter().map(message_model).collect::<Vec<_>>(),
    )));
    window.set_polls(ModelRc::new(VecModel::from(
        chat.polls
            .iter()
            .map(|poll| poll_view_model(poll, &window.get_language().to_string()))
            .collect::<Vec<_>>(),
    )));
    window.set_chat_loading(chat.loading);
    window.set_chat_error_text(chat.error.clone().into());
    window.set_chat_composer_error(chat.composer_error.clone().into());
    window.set_chat_busy(chat.busy);
    window.set_chat_editing(chat.editing);
    window.set_chat_draft(chat.draft.clone().into());
    window.set_chat_has_older(chat.history_has_older);
    window.set_chat_banner_text(chat.banner_text.clone().into());
    let pinned_text = chat
        .pins
        .get(chat.selected_pin)
        .and_then(|pinned_id| {
            chat.messages
                .iter()
                .find(|message| &message.id == pinned_id)
        })
        .map(|message| message.body.clone())
        .unwrap_or_default();
    window.set_chat_pinned_text(pinned_text.into());
    window.set_chat_pin_count(i32::try_from(chat.pins.len()).unwrap_or(i32::MAX));
    window.set_chat_pin_counter(if chat.pins.len() > 1 {
        format!("{} / {}", chat.selected_pin + 1, chat.pins.len()).into()
    } else {
        "".into()
    });
    window.set_chat_search_open(chat.search_open);
    window.set_chat_search_query(chat.search_query.clone().into());
    window.set_chat_search_results(ModelRc::new(VecModel::from(
        chat.search_results
            .iter()
            .map(|result| ChatSearchResult {
                sender: result.sender.clone().into(),
                timestamp: result.timestamp.clone().into(),
                excerpt: result.excerpt.clone().into(),
            })
            .collect::<Vec<_>>(),
    )));
    window.set_chat_searching(false);
    window.set_chat_search_selected_index(chat.search_selected_index);
    window.set_chat_search_position(
        search_position(chat.search_selected_index, chat.search_results.len()).into(),
    );
    // The native app has no media-call adapter yet. Poll creation has a typed
    // core wrapper and is available in group conversations.
    window.set_chat_can_call(false);
    window.set_chat_can_poll(chat.kind == "group" && !chat.loading);
    window.set_chat_can_schedule(false);
    window.set_chat_composer_enabled(
        matches!(chat.kind.as_str(), "direct" | "group") && !chat.loading,
    );
    window.set_polls_loading(chat.polls_loading);
    window.set_polls_has_older(chat.poll_has_older);
    window.set_polls_loading_older(chat.polls_loading_older);
    window.set_polls_error_text(chat.polls_error.clone().into());

    let composer = &chat.poll_composer;
    window.set_chat_poll_create_open(composer.open);
    window.set_chat_poll_create_busy(composer.busy);
    window.set_chat_poll_create_can_create(
        composer.open && !composer.busy && poll_composer_valid(composer),
    );
    window.set_chat_poll_create_is_quiz(composer.is_quiz);
    window.set_chat_poll_create_allows_multiple(composer.allows_multiple);
    window.set_chat_poll_create_allows_revoting(composer.allows_revoting);
    window.set_chat_poll_create_allows_user_options(composer.allows_user_options);
    window.set_chat_poll_create_closes_automatically(composer.closes_automatically);
    window.set_chat_poll_create_can_add_option(composer.options.len() < 15);
    window.set_chat_poll_create_duration_open(composer.duration_dialog_open);
    window.set_chat_poll_create_duration_valid(valid_duration_fields(
        &composer.duration_hours,
        &composer.duration_minutes,
    ));
    window.set_chat_poll_create_duration_label(
        composer
            .open_period_seconds
            .map(|seconds| format!("{} ч {} мин", seconds / 3600, (seconds % 3600) / 60))
            .unwrap_or_else(|| localized("Без ограничения", "No time limit").to_owned())
            .into(),
    );
    window.set_chat_poll_create_title_count_label(
        format!("{} / 64", composer.title.chars().count()).into(),
    );
    window.set_chat_poll_create_description_count_label(
        format!("{} / 128", composer.description.chars().count()).into(),
    );
    window.set_chat_poll_create_error_text(composer.error.clone().into());
    window.set_chat_poll_create_options(ModelRc::new(VecModel::from(
        composer
            .options
            .iter()
            .map(|option| PollDraftOption {
                id: option.id.clone().into(),
                text: option.text.clone().into(),
                correct: composer.correct_option_id.as_deref() == Some(option.id.as_str()),
                can_remove: composer.options.len() > 2,
            })
            .collect::<Vec<_>>(),
    )));
    window.set_chat_poll_create_title(composer.title.clone().into());
    window.set_chat_poll_create_description(composer.description.clone().into());
    window.set_chat_poll_create_duration_hours(composer.duration_hours.clone().into());
    window.set_chat_poll_create_duration_minutes(composer.duration_minutes.clone().into());

    let selected_message = chat
        .message_menu_open
        .then(|| usize::try_from(chat.message_menu_index).ok())
        .flatten()
        .and_then(|index| chat.messages.get(index));
    let menu_items = selected_message
        .filter(|message| message.own)
        .map(|_| {
            vec![
                ChatMenuItem {
                    id: "edit".into(),
                    label: localized("Изменить", "Edit").into(),
                    enabled: !chat.busy && !chat.loading,
                    danger: false,
                },
                ChatMenuItem {
                    id: "delete".into(),
                    label: localized("Удалить", "Delete").into(),
                    enabled: !chat.busy && !chat.loading,
                    danger: true,
                },
            ]
        })
        .unwrap_or_default();
    let quick_reactions = ["👍", "❤️", "😂", "😮", "😢", "🔥"]
        .into_iter()
        .map(|emoji| {
            let count = selected_message
                .map(|message| {
                    message
                        .reactions
                        .iter()
                        .filter(|reaction| reaction.emoji == emoji)
                        .count()
                })
                .unwrap_or(0);
            let mine = selected_message.is_some_and(|message| {
                message
                    .reactions
                    .iter()
                    .any(|reaction| reaction.emoji == emoji && reaction.mine)
            });
            ChatReaction {
                emoji: emoji.into(),
                count_label: count.to_string().into(),
                mine,
            }
        })
        .collect::<Vec<_>>();
    window.set_chat_message_menu_open(chat.message_menu_open);
    window.set_chat_message_menu_index(chat.message_menu_index);
    window.set_chat_message_menu_items(ModelRc::new(VecModel::from(menu_items)));
    window.set_chat_message_menu_reactions(ModelRc::new(VecModel::from(quick_reactions)));
    window.set_chat_delete_confirm_open(chat.delete_confirmation_message_id.is_some());
    window.set_chat_delete_confirm_busy(chat.delete_busy);
    window.set_chat_delete_confirm_error_text(chat.delete_error.clone().into());
    window.set_chat_delete_confirm_title(localized("Удалить сообщение?", "Delete message?").into());
    window.set_chat_delete_confirm_hint(
        localized(
            "Сообщение исчезнет у всех участников. Отменить это нельзя.",
            "The message disappears for everyone in the conversation. This cannot be undone.",
        )
        .into(),
    );
    window.set_chat_delete_confirm_cancel_label(localized("Отмена", "Cancel").into());
    window.set_chat_delete_confirm_delete_label(localized("Удалить", "Delete").into());
    window.set_chat_delete_confirm_busy_label(localized("Удаляем…", "Deleting…").into());
}

fn poll_view_model(poll: &PollState, language: &str) -> PollViewModel {
    let deleted = poll
        .item
        .deleted_option_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut options = Vec::new();
    if let Some(metadata) = &poll.metadata {
        for option in &metadata.options {
            if deleted.contains(option.id.as_str()) {
                continue;
            }
            options.push(poll_option_view(
                &option.id,
                &option.text,
                false,
                false,
                poll,
            ));
        }
    }
    for extra in &poll.item.extra_options {
        if deleted.contains(extra.option_id.as_str()) {
            continue;
        }
        options.push(poll_option_view(
            &extra.option_id,
            poll.extra_texts
                .get(&extra.option_id)
                .map(String::as_str)
                .unwrap_or(""),
            true,
            true,
            poll,
        ));
    }
    let closed = poll.item.closed_at.is_some()
        || poll
            .item
            .closes_at
            .is_some_and(|closes_at| closes_at <= now_unix());
    let has_voted = !poll.item.my_votes.is_empty();
    let can_vote = !closed && (!has_voted || poll.item.allows_revoting);
    let active_selected = &poll.selected;
    let can_submit = !active_selected.is_empty()
        && active_selected.iter().all(|id| {
            options
                .iter()
                .any(|option| option.id.as_str() == id.as_str())
        });
    let title = poll
        .metadata
        .as_ref()
        .map(|metadata| metadata.title.clone())
        .unwrap_or_default();
    let description = poll
        .metadata
        .as_ref()
        .map(|metadata| metadata.description.clone())
        .unwrap_or_default();
    let cannot_decrypt = if poll.metadata.is_none() {
        localized(
            "Не удалось расшифровать опрос на этом устройстве.",
            "This device could not decrypt this poll.",
        )
    } else {
        ""
    };
    let deleted_counts = poll
        .item
        .counts
        .iter()
        .filter(|count| !deleted.contains(count.option_id.as_str()))
        .map(|count| (count.option_id.as_str(), count.count))
        .collect::<HashMap<_, _>>();
    let total = poll.item.total_voters.max(0);
    let total_label = if language == "ru" {
        format!("{} голосов", total)
    } else {
        format!("{} votes", total)
    };
    PollViewModel {
        id: poll.item.id.clone().into(),
        title: title.into(),
        description: description.into(),
        type_label: if poll.item.poll_type == "quiz" {
            localized("Викторина", "Quiz")
        } else {
            localized("Опрос", "Poll")
        }
        .into(),
        total_voters_label: total_label.into(),
        closes_at_label: poll
            .item
            .closes_at
            .map(utc_time_label)
            .unwrap_or_default()
            .into(),
        vote_label: localized("Голосовать", "Vote").into(),
        your_vote_label: localized("Ваш голос учтён", "Your vote is recorded").into(),
        close_label: localized("Завершён", "Closed").into(),
        cannot_decrypt_label: cannot_decrypt.into(),
        option_unavailable_label: localized("Вариант недоступен", "Option unavailable").into(),
        correct_label: localized("Верно", "Correct").into(),
        propose_label: localized("Предложить вариант", "Suggest an option").into(),
        add_option_label: localized("Добавить", "Add").into(),
        stop_label: localized("Завершить", "Close").into(),
        delete_label: localized("Удалить опрос", "Delete poll").into(),
        delete_description: localized(
            "Опрос и его результаты будут удалены.",
            "The poll and its results will be deleted.",
        )
        .into(),
        closed,
        busy: poll.busy,
        error_text: poll.error.clone().into(),
        owner: poll.owner,
        can_vote,
        can_submit_vote: can_vote && can_submit,
        has_voted,
        can_propose: poll.item.allows_user_options && !closed,
        proposal_valid: poll.proposal.trim().chars().count() > 0,
        allows_multiple: poll.item.allows_multiple,
        show_results: closed || has_voted,
        options: ModelRc::new(VecModel::from(
            options
                .into_iter()
                .map(|mut option| {
                    let count = *deleted_counts.get(option.id.as_str()).unwrap_or(&0);
                    let percentage = if total > 0 {
                        ((count * 100) / total).clamp(0, 100) as i32
                    } else {
                        0
                    };
                    option.count_label = count.to_string().into();
                    option.percent_label = format!("{percentage}%").into();
                    option.percentage = percentage;
                    option
                })
                .collect::<Vec<_>>(),
        )),
    }
}

fn poll_option_view(
    id: &str,
    text: &str,
    user_proposed: bool,
    can_remove: bool,
    poll: &PollState,
) -> PollOptionView {
    let deleted = poll
        .item
        .deleted_option_ids
        .iter()
        .any(|deleted| deleted == id);
    let count = poll
        .item
        .counts
        .iter()
        .find(|count| count.option_id == id)
        .map(|count| count.count)
        .unwrap_or(0)
        .max(0);
    let total = poll.item.total_voters.max(0);
    let percentage = if total > 0 {
        (count * 100 / total).clamp(0, 100) as i32
    } else {
        0
    };
    PollOptionView {
        id: id.into(),
        text: text.into(),
        count_label: count.to_string().into(),
        percent_label: format!("{percentage}%").into(),
        percentage,
        selected: poll.selected.iter().any(|selected| selected == id),
        correct: poll.item.correct_option_id.as_deref() == Some(id),
        user_proposed,
        can_remove: can_remove && !deleted,
    }
}

fn message_model(message: &MessageEntry) -> ChatMessage {
    let mut grouped = HashMap::<String, (usize, bool)>::new();
    for reaction in &message.reactions {
        let entry = grouped.entry(reaction.emoji.clone()).or_default();
        entry.0 += 1;
        entry.1 |= reaction.mine;
    }
    let mut reactions = grouped
        .into_iter()
        .map(|(emoji, (count, mine))| ChatReaction {
            emoji: emoji.into(),
            count_label: count.to_string().into(),
            mine,
        })
        .collect::<Vec<_>>();
    reactions.sort_by_key(|reaction| reaction.emoji.to_string());
    ChatMessage {
        id: message.id.clone().into(),
        day_label: message.day_label.clone().into(),
        author: message.author.clone().into(),
        author_tag: message.author_tag.clone().into(),
        body: message.body.clone().into(),
        time_label: message.time_label.clone().into(),
        delivery_label: message.delivery_label.clone().into(),
        reactions: ModelRc::new(VecModel::from(reactions)),
        attachments: empty_model(),
        own: message.own,
        edited: message.edited,
        show_day_label: message.show_day_label,
    }
}

fn normalize_day_markers(messages: &mut [MessageEntry]) {
    messages.sort_by_key(|message| message.server_timestamp);
    let mut previous_day = String::new();
    for message in messages {
        let day = utc_day_label(message.server_timestamp);
        message.day_label = day.clone();
        message.show_day_label = day != previous_day;
        previous_day = day;
    }
}

fn apply_list_from_context(ctx: &AppContext, account_id: &str) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    if session.account.id != account_id {
        return;
    }
    let context = ctx.clone();
    let session_for_ui = session;
    let account_id = account_id.to_owned();
    ctx.apply_ui(move |window| {
        if same_active_session(&context, &session_for_ui) && is_conversation_screen(&window) {
            apply_conversation_list(&context, window, &account_id);
        }
    });
}

fn apply_selected_chat(ctx: &AppContext) {
    let Some(session) = ctx.active_session() else {
        return;
    };
    let account_id = session.account.id.clone();
    let Some(conversation_id) = selected_conversation_id(ctx) else {
        return;
    };
    let context = ctx.clone();
    ctx.apply_ui(move |window| {
        if !same_active_session(&context, &session)
            || !is_chat_route_visible(&window, &conversation_id, false, None)
        {
            return;
        }
        let mut state = state_lock();
        if let Some(chat) = state
            .accounts
            .get_mut(&account_id)
            .and_then(|account| account.chats.get_mut(&conversation_id))
        {
            apply_chat_models(window, chat);
        }
    });
}

fn update_chat_search(chat: &mut ChatState, query: &str) {
    chat.search_query = query.to_owned();
    chat.search_results.clear();
    let needle = query.trim().to_lowercase();
    if !needle.is_empty() {
        chat.search_results = chat
            .messages
            .iter()
            .filter(|message| message.body.to_lowercase().contains(&needle))
            .map(|message| ChatSearchResultEntry {
                sender: message.author.clone(),
                timestamp: message.time_label.clone(),
                excerpt: search_excerpt(&message.body),
            })
            .collect();
    }
    chat.search_selected_index = if chat.search_results.is_empty() {
        -1
    } else {
        0
    };
}

fn search_excerpt(body: &str) -> String {
    let mut excerpt = body.chars().take(120).collect::<String>();
    if body.chars().count() > 120 {
        excerpt.push('…');
    }
    excerpt
}

fn move_search_selection(chat: &mut ChatState, next: bool) {
    let count = chat.search_results.len();
    if count == 0 {
        chat.search_selected_index = -1;
        return;
    }
    let current = usize::try_from(chat.search_selected_index).unwrap_or(0) % count;
    chat.search_selected_index = if next {
        ((current + 1) % count) as i32
    } else {
        (if current == 0 { count - 1 } else { current - 1 }) as i32
    };
}

fn search_position(index: i32, count: usize) -> String {
    usize::try_from(index)
        .ok()
        .filter(|index| *index < count)
        .map(|index| format!("{} / {count}", index + 1))
        .unwrap_or_default()
}

fn conversation_banner(conversations: &[conversations_api::Conversation], id: &str) -> String {
    let Some(conversation) = conversations
        .iter()
        .find(|conversation| conversation.id == id)
    else {
        return String::new();
    };
    let mut notices = Vec::new();
    if conversation.archived_at.is_some() {
        notices.push("Чат находится в архиве");
    }
    if conversation
        .muted_until
        .is_some_and(|muted_until| muted_until > now_unix())
    {
        notices.push("Уведомления отключены");
    }
    notices.join(" · ")
}

fn next_request_generation() -> u64 {
    REQUEST_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1
}

fn begin_list_request(account_id: &str) -> u64 {
    let generation = next_request_generation();
    state_lock()
        .accounts
        .entry(account_id.to_owned())
        .or_default()
        .list_request_generation = generation;
    generation
}

fn begin_account_request(account_id: &str, key: &str) -> u64 {
    let generation = next_request_generation();
    state_lock()
        .accounts
        .entry(account_id.to_owned())
        .or_default()
        .request_generations
        .insert(key.to_owned(), generation);
    generation
}

fn invalidate_navigation_requests(account_id: &str) {
    let mut state = state_lock();
    if let Some(account) = state.accounts.get_mut(account_id) {
        account.request_generations.clear();
        for chat in account.chats.values_mut() {
            chat.request_generations.retain(|key, _| {
                !matches!(
                    key.as_str(),
                    "load" | "history-older" | "polls-reload" | "polls-older"
                )
            });
        }
    }
}

fn begin_chat_request(account_id: &str, conversation_id: &str, key: &str) -> u64 {
    let generation = next_request_generation();
    state_lock()
        .accounts
        .entry(account_id.to_owned())
        .or_default()
        .chats
        .entry(conversation_id.to_owned())
        .or_default()
        .request_generations
        .insert(key.to_owned(), generation);
    generation
}

fn chat_request_generation_is_current(
    account_id: &str,
    conversation_id: &str,
    key: &str,
    generation: u64,
) -> bool {
    state_lock()
        .accounts
        .get(account_id)
        .and_then(|account| account.chats.get(conversation_id))
        .and_then(|chat| chat.request_generations.get(key))
        .is_some_and(|current| *current == generation)
}

fn account_request_generation_is_current(account_id: &str, key: &str, generation: u64) -> bool {
    state_lock()
        .accounts
        .get(account_id)
        .and_then(|account| account.request_generations.get(key))
        .is_some_and(|current| *current == generation)
}

fn list_request_is_current(
    ctx: &AppContext,
    window: &MainWindow,
    session: &ActiveSession,
    account_id: &str,
    expected_route: &str,
    generation: u64,
) -> bool {
    same_active_session(ctx, session)
        && window.get_route().as_str() == expected_route
        && is_conversation_screen(window)
        && state_lock()
            .accounts
            .get(account_id)
            .is_some_and(|account| account.list_request_generation == generation)
}

fn same_active_session(ctx: &AppContext, expected: &ActiveSession) -> bool {
    ctx.active_session().is_some_and(|active| {
        active.account.id == expected.account.id && Arc::ptr_eq(&active.core, &expected.core)
    })
}

fn is_conversation_screen(window: &MainWindow) -> bool {
    matches!(
        window.get_screen().as_str(),
        "chat_index" | "chat" | "channel"
    )
}

fn is_chat_route_visible(
    window: &MainWindow,
    conversation_id: &str,
    allow_pending_selection: bool,
    expected_route: Option<&str>,
) -> bool {
    let route = window.get_route();
    let screen = window.get_screen();
    if let Some(expected_route) = expected_route {
        if route.as_str() != expected_route
            || window.get_selected_conversation_id().as_str() != conversation_id
        {
            return false;
        }
        return match expected_route {
            route if route.starts_with("/app/channel/") => screen.as_str() == "channel",
            route if route.starts_with("/app/chat/") => screen.as_str() == "chat",
            _ => false,
        };
    }
    if allow_pending_selection {
        matches!(screen.as_str(), "chat_index" | "chat" | "channel")
    } else {
        window.get_selected_conversation_id().as_str() == conversation_id
            && matches!(screen.as_str(), "chat" | "channel")
    }
}

fn state_lock() -> std::sync::MutexGuard<'static, ControllerState> {
    CONTROLLER_STATE
        .get_or_init(|| Mutex::new(ControllerState::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn active_account_id(ctx: &AppContext) -> Option<String> {
    ctx.active_session().map(|session| session.account.id)
}

fn selected_conversation_id(ctx: &AppContext) -> Option<String> {
    ctx.ui
        .upgrade()
        .map(|window| window.get_selected_conversation_id().to_string())
        .filter(|id| !id.is_empty())
}

fn conversation_id_from_route(route: &str) -> Option<String> {
    route
        .strip_prefix("/app/chat/")
        .or_else(|| route.strip_prefix("/app/channel/"))
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn row_title(
    conversation: &conversations_api::Conversation,
    cached: Option<&(String, String, usize, Option<String>)>,
) -> String {
    cached
        .map(|(title, _, _, _)| title.clone())
        .filter(|title| !title.is_empty())
        .or_else(|| conversation.title.clone())
        .unwrap_or_else(|| fallback_conversation_title(&conversation.kind, &conversation.id))
}

fn fallback_conversation_title(kind: &str, id: &str) -> String {
    match kind {
        "direct" => localized("Личный чат", "Direct chat").to_owned(),
        "group" => format!("{} {}", localized("Группа", "Group"), short_identifier(id)),
        "channel" => format!("{} {}", localized("Канал", "Channel"), short_identifier(id)),
        _ => short_identifier(id),
    }
}

fn initials(text: &str) -> String {
    text.split_whitespace()
        .take(2)
        .filter_map(|part| part.chars().next())
        .collect::<String>()
        .to_uppercase()
}

fn short_identifier(id: &str) -> String {
    id.chars().take(8).collect()
}

fn localized(ru: &'static str, en: &'static str) -> &'static str {
    // The caller can replace these with localized UI catalogs when the native
    // chat screens gain the same localization contract as the web client.
    let _ = en;
    ru
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn utc_time_label(timestamp: i64) -> String {
    let seconds = timestamp.rem_euclid(86_400);
    format!("{:02}:{:02} UTC", seconds / 3_600, (seconds % 3_600) / 60)
}

fn utc_day_label(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} UTC")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    hex.as_bytes()
        .chunks_exact(2)
        .map(|chunk| {
            let high = hex_nibble(chunk[0])?;
            let low = hex_nibble(chunk[1])?;
            Some((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn uuid_v7() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
        & 0x0000_ffff_ffff_ffff;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64)
        .unwrap_or_default();
    let sequence = MESSAGE_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let process = std::process::id() as u64;
    let random_a = ((sequence ^ nanos ^ process.rotate_left(7)) & 0x0fff) as u16;
    let random_b = (sequence.rotate_left(19).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ nanos.rotate_left(11)
        ^ process)
        & 0x3fff_ffff_ffff_ffff;
    let mut bytes = [0_u8; 16];
    bytes[0..6].copy_from_slice(&millis.to_be_bytes()[2..8]);
    bytes[6] = 0x70 | ((random_a >> 8) as u8 & 0x0f);
    bytes[7] = random_a as u8;
    bytes[8] = 0x80 | ((random_b >> 56) as u8 & 0x3f);
    bytes[9..16].copy_from_slice(&random_b.to_be_bytes()[1..8]);
    let hex = encode_hex(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn empty_model<T: 'static>() -> ModelRc<T> {
    ModelRc::default()
}
