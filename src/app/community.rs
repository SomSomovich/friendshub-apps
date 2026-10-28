//! Native controllers for community, discovery, invitations, and dating.
//!
//! Core calls are synchronous typed wrapper calls, so every operation below is
//! dispatched with [`AppContext::call`]. Slint state is only changed in the
//! event-loop completion. Some current screens do not forward their child
//! properties through `MainWindow`; those missing bindings are intentionally
//! not emulated here.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use friendshub_core::{Client, error::Error as CoreError};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::app::AppContext;
use crate::features::{bots, chat, community as community_api, conversations, discovery_dating};
use crate::{
    ChannelAnalytic, ChannelPost as UiChannelPost, ChatAttachmentView, ChatReaction,
    ConversationInvite, ConversationMember, ConversationOption, CreateSummaryRow, DatingCard,
    DatingProfileDraft, DiscoverEntry, ExistingGroupRow, GroupMemberRow, MainWindow,
};

const DIRECTORY_LIMIT: i64 = 200;
const CHANNEL_POST_PAGE_SIZE: i64 = 20;
const DATING_PAGE_SIZE: i64 = 20;
const REACTIONS_PAGE_SIZE: i64 = 200;
const CONTACT_SEARCH_LIMIT: i64 = 30;
const MAX_AVATAR_BYTES: usize = 5 * 1024 * 1024;
const MAX_AVATAR_BASE64_BYTES: usize = MAX_AVATAR_BYTES.div_ceil(3) * 4;

static STATE: OnceLock<Mutex<ControllerState>> = OnceLock::new();
static CONTACT_SEARCH_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
struct ControllerState {
    account_id: Option<String>,
    directory_is_channel: bool,
    channel_posts: Vec<community_api::ChannelPost>,
    pinned_posts: Vec<community_api::ChannelPost>,
    channel_editing_post_id: Option<String>,
    channel_has_older: bool,
    group_members: Vec<community_api::GroupMember>,
    channel_members: Vec<community_api::ChannelMember>,
    group_bans: Vec<community_api::GroupBan>,
    invites: Vec<community_api::Invite>,
    available_groups: Vec<conversations::Conversation>,
    channel_analytics: Vec<community_api::PostViews>,
    channel_linked_group_id: Option<String>,
    available_bots: Vec<bots::OwnedBot>,
    create_group_contact_query: String,
    create_group_contact_request_id: u64,
    dating_profile: Option<discovery_dating::DiscoverProfile>,
    dating_details: Option<discovery_dating::DiscoverProfileDetails>,
    dating_details_account_id: Option<String>,
    dating_filters: discovery_dating::DiscoverFilters,
    dating_results: Vec<discovery_dating::DiscoverCard>,
    dating_cursor: Option<discovery_dating::DiscoverCursor>,
    dating_search_started: bool,
    pending_delete: Option<(String, bool)>,
}

fn state() -> &'static Mutex<ControllerState> {
    STATE.get_or_init(|| Mutex::new(ControllerState::default()))
}

fn reset_for_account(account_id: &str) {
    if let Ok(mut state) = state().lock() {
        if state.account_id.as_deref() != Some(account_id) {
            *state = ControllerState {
                account_id: Some(account_id.to_owned()),
                ..ControllerState::default()
            };
        }
    }
}

fn account_id(ctx: &AppContext) -> Option<String> {
    let account_id = ctx.active_session()?.account.id;
    reset_for_account(&account_id);
    Some(account_id)
}

fn failure(window: &MainWindow, error: crate::core::CoreCallFailure) {
    window.set_conversations_error(error.user_message().into());
}

fn success(window: &MainWindow) {
    window.set_conversations_error("".into());
}

fn channel_failure(window: &MainWindow, error: crate::core::CoreCallFailure) {
    let message = error.user_message();
    window.set_channel_error_text(message.into());
    failure(window, error);
}

fn channel_success(window: &MainWindow) {
    window.set_channel_error_text("".into());
    success(window);
}

fn report_spawn_failure(ctx: &AppContext) {
    ctx.apply_ui(|window| {
        window.set_conversations_error("Не удалось запустить операцию.".into());
    });
}

fn route_conversation(route: &str) -> Option<(String, bool)> {
    let path = route.split('?').next().unwrap_or(route);
    for (prefix, is_channel) in [("/app/channel/", true), ("/app/chat/", false)] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let id = rest
                .strip_suffix("/settings")
                .unwrap_or(rest)
                .split('/')
                .next()
                .unwrap_or_default();
            if !id.is_empty() {
                return Some((id.to_owned(), is_channel));
            }
        }
    }
    None
}

fn selected_conversation(window: &MainWindow) -> Option<String> {
    let id = window.get_selected_conversation_id().to_string();
    (!id.is_empty()).then_some(id)
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

#[derive(Clone)]
struct DiscoverEntryData {
    conversation_id: String,
    title: String,
    description: String,
    count_label: String,
    initials: String,
}

impl From<DiscoverEntryData> for DiscoverEntry {
    fn from(value: DiscoverEntryData) -> Self {
        Self {
            conversation_id: value.conversation_id.into(),
            title: value.title.into(),
            description: value.description.into(),
            count_label: value.count_label.into(),
            initials: value.initials.into(),
            has_avatar: false,
            avatar_image: slint::Image::default(),
        }
    }
}

#[derive(Clone)]
struct AttachmentData {
    id: String,
    label: String,
}

#[derive(Clone)]
struct ReactionData {
    emoji: String,
    count_label: String,
    mine: bool,
}

#[derive(Clone)]
struct ChannelPostData {
    id: String,
    author: String,
    time_label: String,
    body: String,
    attachments: Vec<AttachmentData>,
    reactions: Vec<ReactionData>,
    discussion_label: String,
    edited: bool,
    pinned: bool,
}

impl From<ChannelPostData> for UiChannelPost {
    fn from(value: ChannelPostData) -> Self {
        Self {
            id: value.id.into(),
            author: value.author.into(),
            time_label: value.time_label.into(),
            body: value.body.into(),
            attachments: model(
                value
                    .attachments
                    .into_iter()
                    .map(|attachment| ChatAttachmentView {
                        id: attachment.id.into(),
                        label: attachment.label.into(),
                        has_preview: false,
                        preview_image: slint::Image::default(),
                        size_label: "".into(),
                        state_label: "".into(),
                        unavailable: false,
                    })
                    .collect(),
            ),
            reactions: model(
                value
                    .reactions
                    .into_iter()
                    .map(|reaction| ChatReaction {
                        emoji: reaction.emoji.into(),
                        count_label: reaction.count_label.into(),
                        mine: reaction.mine,
                    })
                    .collect(),
            ),
            discussion_label: value.discussion_label.into(),
            edited: value.edited,
            pinned: value.pinned,
        }
    }
}

fn channel_post_model(rows: Vec<ChannelPostData>) -> ModelRc<UiChannelPost> {
    model(rows.into_iter().map(UiChannelPost::from).collect())
}

fn channel_reaction_options() -> ModelRc<ChatReaction> {
    model(
        ["👍", "❤️", "😂", "😮", "😢", "🔥"]
            .into_iter()
            .map(|emoji| ChatReaction {
                emoji: emoji.into(),
                count_label: "".into(),
                mine: false,
            })
            .collect(),
    )
}

fn dating_card(
    account_id: String,
    display_name: String,
    age: i32,
    city: String,
    gender: discovery_dating::DiscoverGender,
    looking_for: discovery_dating::DiscoverLookingFor,
    bio: Option<String>,
    interests: Vec<String>,
    avatar_image: Option<slint::Image>,
) -> DatingCard {
    DatingCard {
        account_id: account_id.into(),
        display_name: display_name.clone().into(),
        age: age.to_string().into(),
        city: city.into(),
        gender_label: gender_label(gender).into(),
        looking_for_label: looking_for_label(looking_for).into(),
        initials: initials(&display_name).into(),
        has_avatar: avatar_image.is_some(),
        avatar_image: avatar_image.unwrap_or_default(),
        bio: bio.unwrap_or_default().into(),
        interests_label: interests.join(", ").into(),
    }
}

fn dating_card_from_result(card: discovery_dating::DiscoverCard) -> DatingCard {
    dating_card(
        card.account_id,
        card.display_name,
        card.age,
        card.city,
        card.gender,
        card.looking_for,
        card.bio,
        card.interests,
        None,
    )
}

fn dating_card_from_details(
    details: &discovery_dating::DiscoverProfileDetails,
    avatar_image: Option<slint::Image>,
) -> DatingCard {
    dating_card(
        details.account_id.clone(),
        details.display_name.clone(),
        details.age,
        details.city.clone(),
        details.gender,
        details.looking_for,
        details.bio.clone(),
        details.interests.clone(),
        avatar_image,
    )
}

fn empty_dating_card() -> DatingCard {
    dating_card(
        String::new(),
        String::new(),
        0,
        String::new(),
        discovery_dating::DiscoverGender::Unspecified,
        discovery_dating::DiscoverLookingFor::Friendship,
        None,
        Vec::new(),
        None,
    )
}

fn gender_label(gender: discovery_dating::DiscoverGender) -> &'static str {
    match gender {
        discovery_dating::DiscoverGender::Male => "Мужчина",
        discovery_dating::DiscoverGender::Female => "Женщина",
        discovery_dating::DiscoverGender::Other => "Другое",
        discovery_dating::DiscoverGender::Unspecified => "Не указано",
    }
}

fn looking_for_label(looking_for: discovery_dating::DiscoverLookingFor) -> &'static str {
    match looking_for {
        discovery_dating::DiscoverLookingFor::Friendship => "Дружба",
        discovery_dating::DiscoverLookingFor::Relationship => "Отношения",
        discovery_dating::DiscoverLookingFor::Chat => "Общение",
        discovery_dating::DiscoverLookingFor::All => "Любые знакомства",
    }
}

fn group_member_row(member: &community_api::GroupMember, now: i64) -> ConversationMember {
    ConversationMember {
        id: member.account_id.clone().into(),
        name: member.account_id.clone().into(),
        role: member.role.clone().into(),
        tag: member.group_tag.clone().unwrap_or_default().into(),
        actor_type: "account".into(),
        muted: member
            .muted_until
            .is_some_and(|muted_until| muted_until > now)
            || member
                .moderator_muted_until
                .is_some_and(|muted_until| muted_until > now),
    }
}

fn channel_member_row(member: &community_api::ChannelMember) -> ConversationMember {
    ConversationMember {
        id: member.actor_id.clone().into(),
        name: member.actor_id.clone().into(),
        role: member.role.clone().into(),
        tag: "".into(),
        actor_type: member.actor_type.clone().into(),
        muted: false,
    }
}

fn invite_rows(invites: &[community_api::Invite]) -> Vec<ConversationInvite> {
    invites
        .iter()
        .map(|invite| {
            let kind_label = match invite.kind.as_str() {
                "permanent" => "Постоянная ссылка".to_owned(),
                "one_time" => "Одноразовая ссылка".to_owned(),
                _ => invite.kind.clone(),
            };
            let use_limit = invite
                .max_uses
                .map(|limit| format!(" из {limit}"))
                .unwrap_or_default();
            ConversationInvite {
                id: invite.id.clone().into(),
                kind_label: kind_label.into(),
                token: invite.token.clone().into(),
                detail: format!(
                    "Использовано: {}{} · Создано: {}",
                    invite.used_count,
                    use_limit,
                    format_timestamp(invite.created_at)
                )
                .into(),
            }
        })
        .collect()
}

fn existing_group_rows(
    groups: &[conversations::Conversation],
    selected_id: &str,
) -> Vec<ExistingGroupRow> {
    groups
        .iter()
        .filter(|group| group.kind == "group")
        .map(|group| ExistingGroupRow {
            conversation_id: group.id.clone().into(),
            title: group
                .title
                .clone()
                .unwrap_or_else(|| "Группа".to_owned())
                .into(),
            selected: group.id == selected_id,
        })
        .collect()
}

fn conversation_option(
    id: String,
    label: String,
    selected: bool,
    present: bool,
) -> ConversationOption {
    ConversationOption {
        id: id.into(),
        label: label.into(),
        present,
        selected,
    }
}

fn analytics_rows(
    analytics: &[community_api::PostViews],
    posts: &[community_api::ChannelPost],
) -> Vec<ChannelAnalytic> {
    analytics
        .iter()
        .map(|item| ChannelAnalytic {
            post_id: item.post_id.clone().into(),
            text: posts
                .iter()
                .find(|post| post.id == item.post_id)
                .map(|post| post.text.chars().take(120).collect::<String>())
                .unwrap_or_default()
                .into(),
            views_label: item.views.to_string().into(),
        })
        .collect()
}

fn group_options(
    groups: &[conversations::Conversation],
    linked_group_id: Option<&str>,
) -> Vec<ConversationOption> {
    groups
        .iter()
        .filter(|group| group.kind == "group")
        .map(|group| {
            conversation_option(
                group.id.clone(),
                group.title.clone().unwrap_or_else(|| "Группа".to_owned()),
                linked_group_id == Some(group.id.as_str()),
                false,
            )
        })
        .collect()
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}

fn current_year() -> i64 {
    let days = now_seconds().div_euclid(86_400);
    civil_from_days(days).0
}

fn format_age_label(age: i64) -> String {
    let last_two = age.rem_euclid(100);
    let last = age.rem_euclid(10);
    let unit = if (11..=14).contains(&last_two) {
        "лет"
    } else {
        match last {
            1 => "год",
            2..=4 => "года",
            _ => "лет",
        }
    };
    format!("{age} {unit}")
}

/// Load community-related data for a normalized route such as
/// `/app/channel/<id>` or `/app/chat/<id>/settings`.
pub fn load(ctx: &AppContext, route: &str) {
    let Some(account_id) = account_id(ctx) else {
        return;
    };

    if route.starts_with("/app/discover") {
        let is_channel = state()
            .lock()
            .map(|state| state.directory_is_channel)
            .unwrap_or(false);
        load_directory(ctx, is_channel, None);
        return;
    }

    if route.starts_with("/app/dating") {
        load_dating_profile(ctx);
        return;
    }

    if let Some((conversation_id, is_channel)) = route_conversation(route) {
        if route
            .split('?')
            .next()
            .unwrap_or(route)
            .ends_with("/settings")
        {
            load_conversation_settings(ctx, account_id, conversation_id, is_channel);
        } else if is_channel {
            load_channel(ctx, account_id, conversation_id);
        }
        return;
    }

    if route.starts_with("/app/create/channel") {
        load_existing_groups(ctx, account_id);
    } else if route.starts_with("/app/create/group") {
        load_create_group_state(ctx);
    }
}

fn load_directory(ctx: &AppContext, is_channel: bool, lookup: Option<String>) {
    let lookup_mode = lookup.is_some();
    ctx.apply_ui(move |window| {
        window.set_discover_channel_mode(is_channel);
        window.set_discover_busy(true);
        window.set_discover_loading(true);
        window.set_discover_has_error(false);
        window.set_discover_error_text("".into());
        window.set_discover_lookup_mode(lookup_mode);
    });
    let call = move |client: &Client| {
        let entries = if let Some(handle) = lookup.as_deref() {
            let result = if is_channel {
                discovery_dating::lookup_channel_by_handle(client, handle)?
            } else {
                discovery_dating::lookup_group_by_handle(client, handle)?
            };
            let title = result.title.unwrap_or_else(|| "Без названия".to_owned());
            vec![DiscoverEntryData {
                conversation_id: result.conversation_id,
                title: title.clone(),
                description: result.description.unwrap_or_default(),
                count_label: if is_channel {
                    format!("Подписчики: {}", result.member_count)
                } else {
                    format!("Участники: {}", result.member_count)
                },
                initials: initials(&title),
            }]
        } else {
            let results = if is_channel {
                discovery_dating::list_public_channels(client, Some(DIRECTORY_LIMIT))?
            } else {
                discovery_dating::list_public_groups(client, Some(DIRECTORY_LIMIT))?
            };
            results
                .into_iter()
                .map(|entry| {
                    let title = entry.title.unwrap_or_else(|| "Без названия".to_owned());
                    DiscoverEntryData {
                        conversation_id: entry.conversation_id,
                        initials: initials(&title),
                        title,
                        description: entry.description.unwrap_or_default(),
                        count_label: if is_channel {
                            format!("Подписчики: {}", entry.member_count)
                        } else {
                            format!("Участники: {}", entry.member_count)
                        },
                    }
                })
                .collect()
        };
        Ok(entries)
    };
    let result = ctx.call(
        "friendshub-community-directory",
        call,
        move |window, result| match result {
            Ok(entries) => {
                window.set_discover_entries(model(
                    entries.into_iter().map(DiscoverEntry::from).collect(),
                ));
                window.set_discover_busy(false);
                window.set_discover_loading(false);
                window.set_discover_has_error(false);
                window.set_discover_error_text("".into());
                success(&window);
            }
            Err(error) => {
                window.set_discover_entries(model(Vec::<DiscoverEntry>::new()));
                window.set_discover_busy(false);
                window.set_discover_loading(false);
                window.set_discover_has_error(true);
                window.set_discover_error_text(error.user_message().into());
                failure(&window, error);
            }
        },
    );
    if result.is_err() {
        ctx.apply_ui(|window| {
            window.set_discover_busy(false);
            window.set_discover_loading(false);
            window.set_discover_has_error(true);
            window.set_discover_error_text("Не удалось запустить загрузку каталога.".into());
        });
        report_spawn_failure(ctx);
    }
}

fn load_dating_profile(ctx: &AppContext) {
    ctx.apply_ui(|window| {
        window.set_dating_busy(true);
        window.set_dating_profile_loading(true);
        window.set_dating_profile_error(false);
        window.set_dating_profile_error_text("".into());
        window.set_dating_profile_not_found(false);
        window.set_dating_validation_error("".into());
    });
    let result = ctx.call(
        "friendshub-dating-profile",
        discovery_dating::get_discover_profile,
        move |window, result| match result {
            Ok(profile) => {
                let age = current_year().saturating_sub(i64::from(profile.birth_year));
                if let Ok(mut state) = state().lock() {
                    state.dating_profile = Some(profile.clone());
                }
                window.set_dating_profile_draft(profile_to_draft(&profile));
                window.set_dating_has_profile(true);
                window.set_dating_busy(false);
                window.set_dating_profile_loading(false);
                window.set_dating_profile_error(false);
                window.set_dating_profile_error_text("".into());
                window.set_dating_profile_not_found(false);
                window.set_dating_profile_age_label(format_age_label(age).into());
                window.set_dating_editing(false);
                success(&window);
            }
            Err(crate::core::CoreCallFailure::Server { status: 404 }) => {
                if let Ok(mut state) = state().lock() {
                    state.dating_profile = None;
                    state.dating_results.clear();
                    state.dating_cursor = None;
                    state.dating_search_started = false;
                }
                window.set_dating_profile_draft(empty_profile_draft());
                window.set_dating_has_profile(false);
                window.set_dating_busy(false);
                window.set_dating_profile_loading(false);
                window.set_dating_profile_error(false);
                window.set_dating_profile_error_text("".into());
                window.set_dating_profile_not_found(true);
                window.set_dating_profile_age_label("".into());
                window.set_dating_results(model(Vec::<DatingCard>::new()));
                window.set_dating_search_started(false);
                window.set_dating_has_more(false);
                window.set_dating_editing(false);
                success(&window);
            }
            Err(error) => {
                window.set_dating_busy(false);
                window.set_dating_profile_loading(false);
                window.set_dating_profile_error(true);
                window.set_dating_profile_error_text(error.user_message().into());
                window.set_dating_profile_not_found(false);
                failure(&window, error);
            }
        },
    );
    if result.is_err() {
        ctx.apply_ui(|window| {
            window.set_dating_busy(false);
            window.set_dating_profile_loading(false);
            window.set_dating_profile_error(true);
            window.set_dating_profile_error_text("Не удалось запустить загрузку анкеты.".into());
        });
        report_spawn_failure(ctx);
    }
}

fn load_channel(ctx: &AppContext, account_id: String, conversation_id: String) {
    ctx.apply_ui(|window| {
        window.set_channel_loading(true);
        window.set_channel_busy(true);
        window.set_channel_error_text("".into());
    });
    let actor_id = ctx
        .active_session()
        .map(|session| session.account.id)
        .unwrap_or_default();
    let worker_id = conversation_id.clone();
    let result = ctx.call(
        "friendshub-channel-load",
        move |client| {
            let detail = community_api::channel(client, &worker_id)?;
            let posts = community_api::channel_posts(
                client,
                &worker_id,
                None,
                Some(CHANNEL_POST_PAGE_SIZE),
            )?;
            let pinned = community_api::pinned_posts(client, &worker_id)?;
            let can_publish = community_api::can_publish(client, &worker_id)?;
            let post_rows = posts
                .iter()
                .map(|post| map_channel_post_data(client, post, &actor_id))
                .collect::<Result<Vec<_>, CoreError>>()?;
            let pinned_rows = pinned
                .iter()
                .map(|post| map_channel_post_data(client, post, &actor_id))
                .collect::<Result<Vec<_>, CoreError>>()?;
            Ok((detail, posts, pinned, post_rows, pinned_rows, can_publish))
        },
        move |window, result| match result {
            Ok((detail, posts, pinned, post_rows, pinned_rows, can_publish)) => {
                let has_older = posts.len() == CHANNEL_POST_PAGE_SIZE as usize;
                if let Ok(mut state) = state().lock() {
                    state.channel_posts = posts;
                    state.pinned_posts = pinned;
                    state.channel_has_older = has_older;
                }
                window.set_conversation_title(
                    detail.title.unwrap_or_else(|| "Канал".to_owned()).into(),
                );
                window.set_channel_posts(channel_post_model(post_rows));
                window.set_pinned_channel_posts(channel_post_model(pinned_rows));
                window.set_channel_reaction_options(channel_reaction_options());
                window.set_channel_can_publish(can_publish);
                window.set_channel_has_older(has_older);
                window.set_channel_loading(false);
                window.set_channel_busy(false);
                channel_success(&window);
            }
            Err(error) => {
                window.set_channel_loading(false);
                window.set_channel_busy(false);
                channel_failure(&window, error);
            }
        },
    );
    if result.is_err() {
        let _ = (account_id, conversation_id);
        ctx.apply_ui(|window| {
            window.set_channel_loading(false);
            window.set_channel_busy(false);
            window.set_channel_error_text("Не удалось запустить загрузку канала.".into());
        });
        report_spawn_failure(ctx);
    }
}

fn map_channel_post_data(
    client: &Client,
    post: &community_api::ChannelPost,
    current_account_id: &str,
) -> Result<ChannelPostData, CoreError> {
    let reactions = all_post_reactions(client, &post.id)?;
    let mut by_emoji: BTreeMap<String, (i32, bool)> = BTreeMap::new();
    for reaction in reactions {
        let entry = by_emoji.entry(reaction.emoji).or_default();
        entry.0 += 1;
        entry.1 |= reaction.actor_type == "account" && reaction.actor_id == current_account_id;
    }
    let reactions = by_emoji
        .into_iter()
        .map(|(emoji, (count, mine))| ReactionData {
            emoji,
            count_label: count.to_string(),
            mine,
        })
        .collect();
    let attachments = post
        .attachment_ids
        .iter()
        .enumerate()
        .map(|(index, id)| AttachmentData {
            id: id.clone(),
            label: format!("Вложение {}", index + 1),
        })
        .collect();
    Ok(ChannelPostData {
        id: post.id.clone(),
        author: post.author_id.clone(),
        time_label: format_timestamp(post.created_at),
        body: post.text.clone(),
        attachments,
        reactions,
        discussion_label: if post.discussion_comment_count > 0 {
            format!("Комментарии: {}", post.discussion_comment_count)
        } else {
            String::new()
        },
        edited: post.edited_at.is_some(),
        pinned: post.pinned_at.is_some(),
    })
}

fn all_post_reactions(
    client: &Client,
    post_id: &str,
) -> Result<Vec<community_api::PostReaction>, CoreError> {
    let mut all = Vec::new();
    let mut before_created_at = None;
    let mut before_actor_id: Option<String> = None;
    loop {
        let page = community_api::post_reactions(
            client,
            post_id,
            before_created_at,
            before_actor_id.as_deref(),
            Some(REACTIONS_PAGE_SIZE),
        )?;
        if page.is_empty() {
            break;
        }
        let next_cursor = page
            .last()
            .map(|reaction| (reaction.created_at, reaction.actor_id.clone()));
        let finished = page.len() < REACTIONS_PAGE_SIZE as usize;
        all.extend(page);
        if finished {
            break;
        }
        if let Some((created_at, actor_id)) = next_cursor {
            before_created_at = Some(created_at);
            before_actor_id = Some(actor_id);
        } else {
            break;
        }
    }
    Ok(all)
}

fn load_conversation_settings(
    ctx: &AppContext,
    account_id: String,
    conversation_id: String,
    is_channel: bool,
) {
    ctx.apply_ui(|window| {
        window.set_conversation_settings_loading(true);
        window.set_conversation_settings_no_access(false);
        window.set_conversation_settings_members_unavailable(false);
        window.set_conversation_settings_error_text("".into());
        window.set_conversation_settings_profile_error_text("".into());
        window.set_conversation_settings_busy(false);
    });
    let worker_id = conversation_id.clone();
    let result = ctx.call(
        "friendshub-community-settings",
        move |client| {
            let conversation = conversations::get(client, worker_id.clone())?;
            if is_channel || conversation.kind == "channel" {
                let detail = community_api::channel(client, &worker_id)?;
                let members = community_api::channel_members(client, &worker_id)?;
                let invites = community_api::invites(client, &worker_id)?;
                let analytics = community_api::channel_analytics(client, &worker_id)?;
                let bots = bots::list_owned(client)?;
                Ok(SettingsSnapshot::Channel {
                    detail,
                    members,
                    invites,
                    analytics: analytics.posts,
                    available_bots: bots,
                })
            } else if conversation.kind == "group" {
                let detail = community_api::group(client, &worker_id)?;
                let members = community_api::group_members(client, &worker_id)?;
                let bans = community_api::group_bans(client, &worker_id)?;
                let invites = community_api::invites(client, &worker_id)?;
                Ok(SettingsSnapshot::Group {
                    detail,
                    members,
                    bans,
                    invites,
                })
            } else {
                Ok(SettingsSnapshot::Unsupported(
                    conversation.title.unwrap_or_default(),
                ))
            }
        },
        move |window, result| match result {
            Ok(SettingsSnapshot::Channel {
                detail,
                members,
                invites,
                analytics,
                available_bots,
            }) => {
                let actor_id = account_id.clone();
                let self_member = members
                    .iter()
                    .find(|member| member.actor_type == "account" && member.actor_id == actor_id);
                let is_owner = detail.owner_account_id == actor_id
                    && self_member.is_some_and(|member| member.role == "owner");
                let is_staff = is_owner || self_member.is_some_and(|member| member.role == "admin");
                let cached_groups = state()
                    .lock()
                    .map(|state| state.available_groups.clone())
                    .unwrap_or_default();
                let linked_group_id = detail.linked_group_conversation_id.clone();
                let member_rows: Vec<ConversationMember> =
                    members.iter().map(channel_member_row).collect();
                let invite_model = model(invite_rows(&invites));
                let linked_group_model =
                    model(group_options(&cached_groups, linked_group_id.as_deref()));
                let bot_model = model(
                    available_bots
                        .iter()
                        .map(|bot| {
                            let present = members.iter().any(|member| {
                                member.actor_type == "bot" && member.actor_id == bot.bot_id
                            });
                            let label = bot
                                .handle
                                .as_deref()
                                .map(|handle| format!("@{handle}"))
                                .unwrap_or_else(|| bot.bot_id.clone());
                            conversation_option(bot.bot_id.clone(), label, present, present)
                        })
                        .collect(),
                );
                let post_snapshots = state()
                    .lock()
                    .map(|state| state.channel_posts.clone())
                    .unwrap_or_default();
                let analytics_model = model(analytics_rows(&analytics, &post_snapshots));
                if let Ok(mut state) = state().lock() {
                    state.channel_members = members.clone();
                    state.group_members.clear();
                    state.group_bans.clear();
                    state.invites = invites.clone();
                    state.channel_analytics = analytics.clone();
                    state.channel_linked_group_id = linked_group_id.clone();
                    state.available_bots = available_bots.clone();
                }
                window.set_conversation_title(
                    detail
                        .title
                        .clone()
                        .unwrap_or_else(|| "Канал".to_owned())
                        .into(),
                );
                window.set_conversation_settings_kind("channel".into());
                window.set_conversation_settings_title("Настройки канала".into());
                window.set_conversation_settings_description(
                    detail.description.clone().unwrap_or_default().into(),
                );
                window.set_conversation_settings_handle(
                    detail.handle.clone().unwrap_or_default().into(),
                );
                window.set_conversation_settings_title_draft(
                    detail.title.unwrap_or_else(|| "Канал".to_owned()).into(),
                );
                window.set_conversation_settings_description_draft(
                    detail.description.unwrap_or_default().into(),
                );
                window.set_conversation_settings_handle_draft(
                    detail.handle.unwrap_or_default().into(),
                );
                window.set_conversation_settings_linked_group_id(
                    linked_group_id.clone().unwrap_or_default().into(),
                );
                window.set_conversation_settings_linked_group_draft(
                    linked_group_id.unwrap_or_default().into(),
                );
                window.set_conversation_settings_linked_group_state_known(true);
                window.set_conversation_settings_members(model(member_rows));
                window.set_conversation_settings_banned_members(model(
                    Vec::<ConversationMember>::new(),
                ));
                window.set_conversation_settings_available_members(model(
                    Vec::<ConversationOption>::new(),
                ));
                window.set_conversation_settings_invites(invite_model);
                window.set_conversation_settings_linked_groups(linked_group_model);
                window.set_conversation_settings_available_bots(bot_model);
                window.set_conversation_settings_analytics(analytics_model);
                window.set_conversation_settings_loading(false);
                window.set_conversation_settings_no_access(!is_staff);
                window.set_conversation_settings_members_unavailable(false);
                window.set_conversation_settings_can_edit_profile(is_owner);
                window.set_conversation_settings_can_manage_members(is_owner);
                // The native settings callback has no role editor, mute duration,
                // or tag payload, so those controls cannot be enabled safely.
                window.set_conversation_settings_can_manage_roles(false);
                window.set_conversation_settings_can_mute_members(false);
                window.set_conversation_settings_can_change_tags(false);
                window.set_conversation_settings_can_remove_members(is_owner);
                window.set_conversation_settings_can_ban_members(false);
                window.set_conversation_settings_can_manage_invites(is_staff);
                window.set_conversation_settings_can_view_analytics(is_staff);
                window.set_conversation_settings_is_owner(is_owner);
                window.set_conversation_settings_is_public(detail.is_public);
                window.set_conversation_settings_public_state_draft(detail.is_public);
                window.set_conversation_settings_public_state_known(true);
                window.set_conversation_settings_has_photo(detail.avatar_url.is_some());
                window.set_conversation_settings_profile_save_enabled(is_owner);
                window.set_conversation_settings_confirmation_visible(false);
                window.set_conversation_settings_busy(false);
                success(&window);
            }
            Ok(SettingsSnapshot::Group {
                detail,
                members,
                bans,
                invites,
            }) => {
                let actor_id = account_id.clone();
                let self_member = members.iter().find(|member| member.account_id == actor_id);
                let is_owner = detail.owner_account_id == actor_id
                    && self_member.is_some_and(|member| member.role == "owner");
                let permissions = self_member
                    .map(|member| member.permissions)
                    .unwrap_or_default();
                let has_permission = |bit| is_owner || permissions & bit != 0;
                let is_staff = is_owner || self_member.is_some_and(|member| member.role == "admin");
                let now = now_seconds();
                let member_rows: Vec<ConversationMember> = members
                    .iter()
                    .map(|member| group_member_row(member, now))
                    .collect();
                let banned_rows: Vec<ConversationMember> = bans
                    .iter()
                    .map(|ban| ConversationMember {
                        id: ban.account_id.clone().into(),
                        name: ban.account_id.clone().into(),
                        role: "Заблокирован".into(),
                        tag: ban.reason.clone().unwrap_or_default().into(),
                        actor_type: "account".into(),
                        muted: false,
                    })
                    .collect();
                let invite_model = model(invite_rows(&invites));
                if let Ok(mut state) = state().lock() {
                    state.group_members = members.clone();
                    state.channel_members.clear();
                    state.group_bans = bans.clone();
                    state.invites = invites.clone();
                }
                window.set_conversation_title(
                    detail
                        .title
                        .clone()
                        .unwrap_or_else(|| "Группа".to_owned())
                        .into(),
                );
                window.set_conversation_settings_kind("group".into());
                window.set_conversation_settings_title("Настройки группы".into());
                window.set_conversation_settings_description(
                    detail.description.clone().unwrap_or_default().into(),
                );
                window.set_conversation_settings_handle(
                    detail.handle.clone().unwrap_or_default().into(),
                );
                window.set_conversation_settings_title_draft(
                    detail.title.unwrap_or_else(|| "Группа".to_owned()).into(),
                );
                window.set_conversation_settings_description_draft(
                    detail.description.unwrap_or_default().into(),
                );
                window.set_conversation_settings_handle_draft(
                    detail.handle.unwrap_or_default().into(),
                );
                window.set_conversation_settings_linked_group_id("".into());
                window.set_conversation_settings_linked_group_draft("".into());
                window.set_conversation_settings_linked_group_state_known(false);
                window.set_conversation_settings_members(model(member_rows));
                window.set_conversation_settings_banned_members(model(banned_rows));
                window.set_conversation_settings_available_members(model(
                    Vec::<ConversationOption>::new(),
                ));
                window.set_conversation_settings_invites(invite_model);
                window.set_conversation_settings_linked_groups(model(
                    Vec::<ConversationOption>::new(),
                ));
                window.set_conversation_settings_available_bots(model(
                    Vec::<ConversationOption>::new(),
                ));
                window.set_conversation_settings_analytics(model(Vec::<ChannelAnalytic>::new()));
                window.set_conversation_settings_loading(false);
                window.set_conversation_settings_no_access(!is_staff);
                window.set_conversation_settings_members_unavailable(false);
                window.set_conversation_settings_can_edit_profile(has_permission(
                    community_api::group_permissions::CHANGE_PROFILE,
                ));
                window.set_conversation_settings_can_manage_members(is_staff);
                // Native callbacks do not carry editable permissions, mute
                // duration, or a tag value; keep those controls unavailable.
                window.set_conversation_settings_can_manage_roles(false);
                window.set_conversation_settings_can_mute_members(false);
                window.set_conversation_settings_can_change_tags(false);
                window.set_conversation_settings_can_remove_members(has_permission(
                    community_api::group_permissions::BAN_USERS,
                ));
                window.set_conversation_settings_can_ban_members(has_permission(
                    community_api::group_permissions::BAN_USERS,
                ));
                window.set_conversation_settings_can_manage_invites(has_permission(
                    community_api::group_permissions::MANAGE_INVITES,
                ));
                window.set_conversation_settings_can_view_analytics(false);
                window.set_conversation_settings_is_owner(is_owner);
                window.set_conversation_settings_is_public(detail.is_public);
                window.set_conversation_settings_public_state_draft(detail.is_public);
                window.set_conversation_settings_public_state_known(true);
                window.set_conversation_settings_has_photo(detail.avatar_url.is_some());
                window.set_conversation_settings_profile_save_enabled(has_permission(
                    community_api::group_permissions::CHANGE_PROFILE,
                ));
                window.set_conversation_settings_confirmation_visible(false);
                window.set_conversation_settings_busy(false);
                success(&window);
            }
            Ok(SettingsSnapshot::Unsupported(title)) => {
                window.set_conversation_settings_loading(false);
                window.set_conversation_settings_no_access(false);
                window.set_conversation_title(title.into());
                window.set_conversations_error(
                    "Эти настройки не поддерживаются для данного типа беседы.".into(),
                );
            }
            Err(error) => {
                window.set_conversation_settings_loading(false);
                window.set_conversation_settings_no_access(false);
                window.set_conversation_settings_error_text(error.user_message().into());
                failure(&window, error);
            }
        },
    );
    if result.is_err() {
        let _ = conversation_id;
        ctx.apply_ui(|window| {
            window.set_conversation_settings_loading(false);
            window.set_conversation_settings_error_text(
                "Не удалось запустить загрузку настроек.".into(),
            );
        });
        report_spawn_failure(ctx);
    }
}

enum SettingsSnapshot {
    Group {
        detail: community_api::GroupDetail,
        members: Vec<community_api::GroupMember>,
        bans: Vec<community_api::GroupBan>,
        invites: Vec<community_api::Invite>,
    },
    Channel {
        detail: community_api::ChannelDetail,
        members: Vec<community_api::ChannelMember>,
        invites: Vec<community_api::Invite>,
        analytics: Vec<community_api::PostViews>,
        available_bots: Vec<bots::OwnedBot>,
    },
    Unsupported(String),
}

fn load_existing_groups(ctx: &AppContext, _account_id: String) {
    ctx.apply_ui(|window| {
        window.set_create_channel_groups_loading(true);
        window.set_create_channel_existing_groups(model(Vec::<ExistingGroupRow>::new()));
        window.set_create_channel_discussion_ready(true);
        window.set_create_channel_busy(false);
        window.set_create_channel_handle_feedback("".into());
        window.set_create_channel_handle_feedback_error(false);
        window.set_create_channel_discussion_handle_feedback("".into());
        window.set_create_channel_discussion_handle_feedback_error(false);
        window.set_conversations_error("".into());
    });
    let result = ctx.call(
        "friendshub-channel-create-groups",
        conversations::list,
        move |window, result| match result {
            Ok(items) => {
                let groups = items
                    .into_iter()
                    .filter(|item| item.kind == "group")
                    .collect::<Vec<_>>();
                if let Ok(mut state) = state().lock() {
                    state.group_members.clear();
                    state.channel_members.clear();
                    state.available_groups = groups;
                }
                let groups = state()
                    .lock()
                    .map(|state| state.available_groups.clone())
                    .unwrap_or_default();
                let selected_id = window.get_create_channel_selected_group_id().to_string();
                window.set_create_channel_existing_groups(model(existing_group_rows(
                    &groups,
                    &selected_id,
                )));
                window.set_create_channel_groups_loading(false);
                update_create_channel_draft_state(&window);
                success(&window);
            }
            Err(error) => {
                window.set_create_channel_groups_loading(false);
                window.set_create_channel_discussion_ready(false);
                failure(&window, error);
            }
        },
    );
    if result.is_err() {
        ctx.apply_ui(|window| {
            window.set_create_channel_groups_loading(false);
            window.set_create_channel_discussion_ready(false);
        });
        report_spawn_failure(ctx);
    }
}

fn update_create_channel_draft_state(window: &MainWindow) {
    let title = window.get_create_channel_title().to_string();
    let discussion_kind = window.get_create_channel_discussion_kind().to_string();
    let discussion_title = window.get_create_channel_discussion_title().to_string();
    let selected_group_id = window.get_create_channel_selected_group_id().to_string();
    let groups: Vec<ExistingGroupRow> =
        window.get_create_channel_existing_groups().iter().collect();
    let selected_group = groups
        .iter()
        .find(|group| group.conversation_id.as_str() == selected_group_id);
    let (discussion_summary, discussion_ready) = match discussion_kind.as_str() {
        "existing" => match selected_group {
            Some(group) => (group.title.to_string(), true),
            None => ("Выберите группу для обсуждения".to_owned(), false),
        },
        "new" if !discussion_title.trim().is_empty() => (discussion_title.trim().to_owned(), true),
        "new" => ("Введите название группы обсуждения".to_owned(), false),
        _ => ("Нет".to_owned(), true),
    };
    window.set_create_channel_existing_groups(model(
        groups
            .into_iter()
            .map(|mut group| {
                group.selected = group.conversation_id.as_str() == selected_group_id;
                group
            })
            .collect(),
    ));
    window.set_create_channel_discussion_summary(discussion_summary.into());
    window.set_create_channel_discussion_ready(discussion_ready);
    window.set_create_channel_can_continue(!title.trim().is_empty());
    window.set_create_channel_can_create(!title.trim().is_empty() && discussion_ready);
}

fn load_create_group_state(ctx: &AppContext) {
    let request_id = CONTACT_SEARCH_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut state) = state().lock() {
        state.create_group_contact_query.clear();
        state.create_group_contact_request_id = request_id;
    }
    ctx.apply_ui(|window| {
        window.set_create_group_contacts(model(Vec::<GroupMemberRow>::new()));
        window.set_create_group_selected_contacts(model(Vec::<GroupMemberRow>::new()));
        window.set_create_group_contact_search("".into());
        window.set_create_group_contacts_loading(false);
        window.set_create_group_busy(false);
        window.set_create_group_handle_feedback("".into());
        window.set_create_group_handle_feedback_error(false);
        window.set_conversations_error("".into());
        update_create_group_draft_state(&window);
    });
}

fn update_create_group_draft_state(window: &MainWindow) {
    let title = window.get_create_group_title().to_string();
    let is_public = window.get_create_group_is_public();
    let selected_contacts: Vec<GroupMemberRow> =
        window.get_create_group_selected_contacts().iter().collect();
    let participant_count = selected_contacts.len();
    let title_summary = if title.trim().is_empty() {
        "Не указано".to_owned()
    } else {
        title.trim().to_owned()
    };
    window.set_create_group_summary_rows(model(vec![
        CreateSummaryRow {
            label: "Название".into(),
            value: title_summary.into(),
        },
        CreateSummaryRow {
            label: "Доступ".into(),
            value: if is_public {
                "Публичная"
            } else {
                "Приватная"
            }
            .into(),
        },
        CreateSummaryRow {
            label: "Участники".into(),
            value: participant_count.to_string().into(),
        },
    ]));
    window.set_create_group_selected_members_note(if participant_count == 0 {
        "Группу можно создать без участников.".into()
    } else {
        format!("Выбрано участников: {participant_count}").into()
    });
    window.set_create_group_can_continue(!title.trim().is_empty());
    window.set_create_group_can_create(!title.trim().is_empty());
}

fn toggle_create_group_member(window: &MainWindow, account_id: &str) {
    let contacts: Vec<GroupMemberRow> = window.get_create_group_contacts().iter().collect();
    let selected: Vec<GroupMemberRow> =
        window.get_create_group_selected_contacts().iter().collect();
    if !contacts
        .iter()
        .any(|contact| contact.account_id.as_str() == account_id)
    {
        return;
    }
    let currently_selected = selected
        .iter()
        .any(|contact| contact.account_id.as_str() == account_id);
    let updated_contacts: Vec<GroupMemberRow> = contacts
        .into_iter()
        .map(|mut contact| {
            if contact.account_id.as_str() == account_id {
                contact.selected = !currently_selected;
            }
            contact
        })
        .collect();
    let updated_selected: Vec<GroupMemberRow> = updated_contacts
        .iter()
        .filter(|contact| contact.selected)
        .cloned()
        .collect();
    window.set_create_group_contacts(model(updated_contacts));
    window.set_create_group_selected_contacts(model(updated_selected));
    update_create_group_draft_state(window);
}

/// Dispatch one string-based UI action. Returns `true` for actions owned by
/// this controller; `false` leaves the action for another controller.
pub fn handle(
    ctx: &AppContext,
    namespace: &str,
    action: &str,
    arg1: &str,
    arg2: &str,
    arg3: &str,
) -> bool {
    match namespace {
        "discover" => handle_discover(ctx, action, arg1),
        "dating" => handle_dating(ctx, action, arg1),
        "channel" => handle_channel(ctx, action, arg1, arg2),
        "conversation-settings" => handle_conversation_settings(ctx, action, arg1, arg2, arg3),
        "invites" | "invite" => handle_invites(ctx, action, arg1),
        "create-group" => handle_create_group(ctx, action, arg1),
        "create-channel" => handle_create_channel(ctx, action, arg1),
        _ => false,
    }
}

fn handle_create_group(ctx: &AppContext, action: &str, arg1: &str) -> bool {
    let Some(account_id) = account_id(ctx) else {
        return false;
    };
    match action {
        "contact-search" => {
            search_create_group_contacts(ctx, account_id, arg1.to_owned());
            true
        }
        "member-toggle" => {
            let account_id = arg1.to_owned();
            ctx.apply_ui(move |window| toggle_create_group_member(&window, &account_id));
            true
        }
        _ => false,
    }
}

fn search_create_group_contacts(ctx: &AppContext, account_id: String, query: String) {
    let normalized_query = query.trim().to_owned();
    let should_search = normalized_query.chars().count() >= 3;
    let request_id = CONTACT_SEARCH_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut state) = state().lock() {
        state.create_group_contact_query = normalized_query.clone();
        state.create_group_contact_request_id = request_id;
    }
    ctx.apply_ui({
        let query = query.clone();
        move |window| {
            window.set_create_group_contact_search(query.into());
            window.set_create_group_contacts_loading(should_search);
            window.set_conversations_error("".into());
        }
    });

    if !should_search {
        if contact_search_is_current(&account_id, &normalized_query, request_id) {
            let expected_account_id = account_id.clone();
            let expected_query = normalized_query.clone();
            ctx.apply_ui(move |window| {
                if !is_create_group_route(&window)
                    || !contact_search_is_current(&expected_account_id, &expected_query, request_id)
                {
                    return;
                }
                window.set_create_group_contacts(model(Vec::<GroupMemberRow>::new()));
                window.set_create_group_contacts_loading(false);
                update_create_group_draft_state(&window);
            });
        }
        return;
    }

    let expected_account_id = account_id.clone();
    let expected_query = normalized_query.clone();
    let worker_query = normalized_query.clone();
    let result = ctx.call(
        "friendshub-create-group-contact-search",
        move |client| {
            chat::search_contacts(
                client,
                &chat::ContactSearchRequest {
                    q: worker_query,
                    limit: Some(CONTACT_SEARCH_LIMIT),
                },
            )
        },
        move |window, result| match result {
            Ok(results) => {
                if !contact_search_is_current(&expected_account_id, &expected_query, request_id)
                    || !is_create_group_route(&window)
                {
                    return;
                }
                let mut selected: Vec<GroupMemberRow> =
                    window.get_create_group_selected_contacts().iter().collect();
                let rows: Vec<GroupMemberRow> = results
                    .into_iter()
                    .map(|contact| {
                        let name = contact.local_username.trim();
                        let name = nonempty(name)
                            .or_else(|| nonempty(&contact.username))
                            .unwrap_or_else(|| contact.fh_number.clone());
                        let is_selected = selected.iter().any(|item| {
                            item.account_id.as_str() == contact.target_account_id.as_str()
                        });
                        let row = GroupMemberRow {
                            account_id: contact.target_account_id.clone().into(),
                            name: name.clone().into(),
                            fh_number: contact.fh_number.into(),
                            selected: is_selected,
                            initial: initials(&name).into(),
                        };
                        if is_selected {
                            if let Some(selected_row) = selected.iter_mut().find(|item| {
                                item.account_id.as_str() == contact.target_account_id.as_str()
                            }) {
                                *selected_row = row.clone();
                            }
                        }
                        row
                    })
                    .collect();
                window.set_create_group_contacts(model(rows));
                window.set_create_group_selected_contacts(model(selected));
                window.set_create_group_contacts_loading(false);
                update_create_group_draft_state(&window);
                success(&window);
            }
            Err(error) => {
                if !contact_search_is_current(&expected_account_id, &expected_query, request_id)
                    || !is_create_group_route(&window)
                {
                    return;
                }
                window.set_create_group_contacts(model(Vec::<GroupMemberRow>::new()));
                window.set_create_group_contacts_loading(false);
                window.set_conversations_error(error.user_message().into());
            }
        },
    );
    if result.is_err() && contact_search_is_current(&account_id, &normalized_query, request_id) {
        let expected_account_id = account_id.clone();
        let expected_query = normalized_query.clone();
        ctx.apply_ui(move |window| {
            if !is_create_group_route(&window)
                || !contact_search_is_current(&expected_account_id, &expected_query, request_id)
            {
                return;
            }
            window.set_create_group_contacts(model(Vec::<GroupMemberRow>::new()));
            window.set_create_group_contacts_loading(false);
            window.set_conversations_error("Не удалось запустить поиск контактов.".into());
        });
    }
}

fn is_create_group_route(window: &MainWindow) -> bool {
    window.get_route().split('?').next() == Some("/app/create/group")
}

fn contact_search_is_current(account_id: &str, query: &str, request_id: u64) -> bool {
    state()
        .lock()
        .map(|state| {
            state.account_id.as_deref() == Some(account_id)
                && state.create_group_contact_query == query
                && state.create_group_contact_request_id == request_id
        })
        .unwrap_or(false)
}

fn handle_create_channel(ctx: &AppContext, action: &str, arg1: &str) -> bool {
    if account_id(ctx).is_none() {
        return false;
    }
    match action {
        "discussion-kind" => {
            let kind = arg1.to_owned();
            ctx.apply_ui(move |window| {
                window.set_create_channel_discussion_kind(kind.into());
                update_create_channel_draft_state(&window);
            });
            true
        }
        "select-group" => {
            let group_id = arg1.to_owned();
            ctx.apply_ui(move |window| {
                window.set_create_channel_selected_group_id(group_id.into());
                update_create_channel_draft_state(&window);
            });
            true
        }
        _ => false,
    }
}

fn handle_discover(ctx: &AppContext, action: &str, arg1: &str) -> bool {
    let Some(_account_id) = account_id(ctx) else {
        return false;
    };
    match action {
        "channel-mode" => {
            let is_channel = arg1 == "true";
            if let Ok(mut state) = state().lock() {
                state.directory_is_channel = is_channel;
            }
            load_directory(ctx, is_channel, None);
            true
        }
        "catalog" | "show-catalog" => {
            let is_channel = state()
                .lock()
                .map(|state| state.directory_is_channel)
                .unwrap_or(false);
            ctx.apply_ui(|window| {
                window.set_discover_query("".into());
                window.set_discover_lookup_mode(false);
            });
            load_directory(ctx, is_channel, None);
            true
        }
        "lookup" => {
            let is_channel = state()
                .lock()
                .map(|state| state.directory_is_channel)
                .unwrap_or(false);
            let handle = arg1.trim().trim_start_matches('@').to_owned();
            load_directory(ctx, is_channel, Some(handle));
            true
        }
        "open" => {
            let is_channel = state()
                .lock()
                .map(|state| state.directory_is_channel)
                .unwrap_or(false);
            let conversation_id = arg1.to_owned();
            let worker_id = conversation_id.clone();
            ctx.apply_ui(|window| {
                window.set_discover_busy(true);
                window.set_discover_has_error(false);
                window.set_discover_error_text("".into());
            });
            let result = ctx.call(
                "friendshub-discover-open",
                move |client| {
                    let joined = conversations::list(client)?
                        .iter()
                        .any(|conversation| conversation.id == worker_id);
                    if !joined {
                        if is_channel {
                            discovery_dating::subscribe_channel(client, &worker_id)?;
                        } else {
                            discovery_dating::join_public_group(client, &worker_id)?;
                        }
                    }
                    Ok(())
                },
                move |window, result| match result {
                    Ok(()) => {
                        window.set_discover_busy(false);
                        window.set_discover_has_error(false);
                        window.set_discover_error_text("".into());
                        success(&window);
                        window.invoke_navigate_requested(
                            if is_channel {
                                format!("/app/channel/{conversation_id}")
                            } else {
                                format!("/app/chat/{conversation_id}")
                            }
                            .into(),
                        );
                    }
                    Err(error) => {
                        window.set_discover_busy(false);
                        window.set_discover_has_error(true);
                        window.set_discover_error_text(error.user_message().into());
                        failure(&window, error);
                    }
                },
            );
            if result.is_err() {
                report_spawn_failure(ctx);
            }
            true
        }
        _ => false,
    }
}

fn handle_dating(ctx: &AppContext, action: &str, arg1: &str) -> bool {
    let Some(_account_id) = account_id(ctx) else {
        return false;
    };
    match action {
        "profile-retry" => {
            load_dating_profile(ctx);
            true
        }
        "clear-filters" => {
            if let Ok(mut state) = state().lock() {
                state.dating_filters = discovery_dating::DiscoverFilters::default();
                state.dating_results.clear();
                state.dating_cursor = None;
                state.dating_search_started = false;
            }
            ctx.apply_ui(|window| {
                window.set_dating_filters(crate::DatingSearchFilters::default());
                window.set_dating_results(model(Vec::<DatingCard>::new()));
                window.set_dating_search_started(false);
                window.set_dating_has_more(false);
                window.set_dating_search_error("".into());
                success(&window);
            });
            true
        }
        "load-more" => {
            let Ok(state) = state().lock() else {
                return true;
            };
            let Some(cursor) = state.dating_cursor.clone() else {
                return true;
            };
            let filters = state.dating_filters.clone();
            drop(state);
            load_dating_page(ctx, filters, Some(cursor), true);
            true
        }
        "open-details" => {
            load_dating_details(ctx, arg1.to_owned());
            true
        }
        "details-retry" => {
            let account_id = state()
                .lock()
                .ok()
                .and_then(|state| state.dating_details_account_id.clone());
            if let Some(account_id) = account_id {
                load_dating_details(ctx, account_id);
            }
            true
        }
        "close-details" => {
            if let Ok(mut state) = state().lock() {
                state.dating_details = None;
                state.dating_details_account_id = None;
            }
            ctx.apply_ui(|window| {
                window.set_dating_details_open(false);
                window.set_dating_details_loading(false);
                window.set_dating_details(empty_dating_card());
                window.set_dating_details_fh_number("".into());
                window.set_dating_details_error("".into());
                window.set_dating_busy(false);
            });
            true
        }
        "profile-listed" => {
            let listed = arg1 == "true";
            ctx.apply_ui(move |window| {
                let mut draft = window.get_dating_profile_draft();
                draft.is_listed = listed;
                window.set_dating_profile_draft(draft);
                window.set_dating_busy(true);
                window.set_dating_validation_error("".into());
            });
            let profile = state()
                .lock()
                .ok()
                .and_then(|state| state.dating_profile.clone());
            let Some(profile) = profile else {
                ctx.apply_ui(|window| {
                    window.set_dating_busy(false);
                    window.set_dating_profile_draft(empty_profile_draft());
                });
                return true;
            };
            let fallback_profile = profile.clone();
            let spawn_fallback_profile = fallback_profile.clone();
            let result = ctx.call(
                "friendshub-dating-listing",
                move |client| {
                    if listed {
                        discovery_dating::save_discover_profile(
                            client,
                            &profile_to_input(&profile, true),
                        )
                    } else {
                        discovery_dating::hide_discover_profile(client)?;
                        let mut hidden = profile;
                        hidden.is_listed = false;
                        Ok(hidden)
                    }
                },
                move |window, result| match result {
                    Ok(profile) => {
                        if let Ok(mut state) = state().lock() {
                            state.dating_profile = Some(profile.clone());
                        }
                        window.set_dating_profile_draft(profile_to_draft(&profile));
                        window.set_dating_busy(false);
                        window.set_dating_validation_error("".into());
                        success(&window);
                    }
                    Err(error) => {
                        window.set_dating_profile_draft(profile_to_draft(&fallback_profile));
                        window.set_dating_busy(false);
                        window.set_dating_validation_error(error.user_message().into());
                        failure(&window, error);
                    }
                },
            );
            if result.is_err() {
                ctx.apply_ui(move |window| {
                    window.set_dating_profile_draft(profile_to_draft(&spawn_fallback_profile));
                    window.set_dating_busy(false);
                    window.set_dating_validation_error(
                        "Не удалось запустить изменение видимости анкеты.".into(),
                    );
                });
                report_spawn_failure(ctx);
            }
            true
        }
        "profile-gender" => {
            let Some(index) = arg1.parse::<usize>().ok() else {
                return true;
            };
            let gender = match index {
                0 => "unspecified",
                1 => "female",
                2 => "male",
                3 => "other",
                _ => return true,
            };
            ctx.apply_ui(move |window| {
                let mut draft = window.get_dating_profile_draft();
                draft.gender = gender.into();
                window.set_dating_profile_draft(draft);
            });
            true
        }
        "profile-looking-for" => {
            let Some(index) = arg1.parse::<usize>().ok() else {
                return true;
            };
            let looking_for = match index {
                0 => "friendship",
                1 => "relationship",
                2 => "chat",
                3 => "all",
                _ => return true,
            };
            ctx.apply_ui(move |window| {
                let mut draft = window.get_dating_profile_draft();
                draft.looking_for = looking_for.into();
                window.set_dating_profile_draft(draft);
            });
            true
        }
        "filter-gender" => {
            let Some(index) = arg1.parse::<usize>().ok() else {
                return true;
            };
            let gender = match index {
                0 => "",
                1 => "female",
                2 => "male",
                3 => "other",
                _ => return true,
            };
            ctx.apply_ui(move |window| {
                let mut filters = window.get_dating_filters();
                filters.gender = gender.into();
                window.set_dating_filters(filters);
            });
            true
        }
        "filter-looking-for" => {
            let Some(index) = arg1.parse::<usize>().ok() else {
                return true;
            };
            let looking_for = match index {
                0 => "",
                1 => "friendship",
                2 => "relationship",
                3 => "chat",
                4 => "all",
                _ => return true,
            };
            ctx.apply_ui(move |window| {
                let mut filters = window.get_dating_filters();
                filters.looking_for = looking_for.into();
                window.set_dating_filters(filters);
            });
            true
        }
        "edit-profile" => {
            ctx.apply_ui(|window| {
                window.set_dating_editing(true);
                window.set_dating_validation_error("".into());
            });
            true
        }
        "cancel-edit" => {
            let profile = state()
                .lock()
                .ok()
                .and_then(|state| state.dating_profile.clone());
            ctx.apply_ui(move |window| {
                if let Some(profile) = profile {
                    window.set_dating_profile_draft(profile_to_draft(&profile));
                } else {
                    window.set_dating_profile_draft(empty_profile_draft());
                }
                window.set_dating_editing(false);
                window.set_dating_validation_error("".into());
            });
            true
        }
        _ => false,
    }
}

fn load_dating_details(ctx: &AppContext, account_id: String) {
    if let Ok(mut state) = state().lock() {
        state.dating_details_account_id = Some(account_id.clone());
        state.dating_details = None;
    }
    ctx.apply_ui(|window| {
        window.set_dating_details_open(true);
        window.set_dating_details_loading(true);
        window.set_dating_details_error("".into());
        window.set_dating_details(empty_dating_card());
        window.set_dating_details_fh_number("".into());
        window.set_dating_busy(true);
    });
    let expected_account_id = account_id.clone();
    let spawn_account_id = expected_account_id.clone();
    let result = ctx.call(
        "friendshub-dating-details",
        move |client| {
            let details = discovery_dating::get_discover_profile_details(client, &account_id)?;
            let avatar_bytes = details
                .avatar_url
                .as_deref()
                .filter(|url| !url.trim().is_empty())
                .and_then(|_| {
                    let avatar =
                        discovery_dating::get_account_avatar(client, &details.account_id).ok()?;
                    if avatar.account_id != details.account_id
                        || !avatar.content_type.eq_ignore_ascii_case("image/webp")
                        || avatar.data_base64.len() > MAX_AVATAR_BASE64_BYTES
                    {
                        return None;
                    }
                    base64::engine::general_purpose::STANDARD
                        .decode(avatar.data_base64)
                        .ok()
                        .filter(|bytes| !bytes.is_empty() && bytes.len() <= MAX_AVATAR_BYTES)
                });
            Ok((details, avatar_bytes))
        },
        move |window, result| match result {
            Ok((details, avatar_bytes)) => {
                let details_for_ui = details.clone();
                if let Ok(mut state) = state().lock() {
                    if state.dating_details_account_id.as_deref()
                        != Some(expected_account_id.as_str())
                    {
                        return;
                    }
                    state.dating_details = Some(details);
                } else {
                    return;
                }
                let avatar_image = avatar_bytes
                    .as_deref()
                    .and_then(|bytes| slint::Image::load_from_data(bytes, Some("webp")).ok());
                window.set_dating_details_open(true);
                window.set_dating_details_loading(false);
                window.set_dating_details(dating_card_from_details(&details_for_ui, avatar_image));
                window.set_dating_details_fh_number(details_for_ui.fh_number.into());
                window.set_dating_details_error("".into());
                window.set_dating_busy(false);
                success(&window);
            }
            Err(error) => {
                let is_current = state()
                    .lock()
                    .map(|state| {
                        state.dating_details_account_id.as_deref()
                            == Some(expected_account_id.as_str())
                    })
                    .unwrap_or(false);
                if is_current {
                    window.set_dating_details_open(true);
                    window.set_dating_details_loading(false);
                    window.set_dating_details_error(error.user_message().into());
                    window.set_dating_busy(false);
                    failure(&window, error);
                }
            }
        },
    );
    if result.is_err() {
        let is_current = state()
            .lock()
            .map(|state| {
                state.dating_details_account_id.as_deref() == Some(spawn_account_id.as_str())
            })
            .unwrap_or(false);
        if is_current {
            ctx.apply_ui(|window| {
                window.set_dating_details_loading(false);
                window.set_dating_details_error("Не удалось запустить загрузку анкеты.".into());
                window.set_dating_busy(false);
            });
            report_spawn_failure(ctx);
        }
    }
}

/// Handle `DatingScreen::profile-save-requested` from the typed Slint callback.
pub fn dating_profile_save_requested(ctx: &AppContext, draft: DatingProfileDraft) -> bool {
    let Some(_account_id) = account_id(ctx) else {
        return false;
    };
    let Some(input) = profile_input_from_draft(&draft) else {
        ctx.apply_ui(|window| {
            window.set_dating_validation_error("Проверьте данные анкеты.".into());
        });
        return true;
    };
    ctx.apply_ui(|window| {
        window.set_dating_busy(true);
        window.set_dating_validation_error("".into());
    });
    let result = ctx.call(
        "friendshub-dating-save",
        move |client| discovery_dating::save_discover_profile(client, &input),
        move |window, result| match result {
            Ok(profile) => {
                let age = current_year().saturating_sub(i64::from(profile.birth_year));
                if let Ok(mut state) = state().lock() {
                    state.dating_profile = Some(profile.clone());
                }
                window.set_dating_profile_draft(profile_to_draft(&profile));
                window.set_dating_has_profile(true);
                window.set_dating_busy(false);
                window.set_dating_profile_not_found(false);
                window.set_dating_profile_error(false);
                window.set_dating_profile_error_text("".into());
                window.set_dating_profile_age_label(format_age_label(age).into());
                window.set_dating_editing(false);
                window.set_dating_validation_error("".into());
                success(&window);
            }
            Err(error) => {
                window.set_dating_busy(false);
                window.set_dating_validation_error(error.user_message().into());
                failure(&window, error);
            }
        },
    );
    if result.is_err() {
        ctx.apply_ui(|window| {
            window.set_dating_busy(false);
            window.set_dating_validation_error("Не удалось запустить сохранение анкеты.".into());
        });
        report_spawn_failure(ctx);
    }
    true
}

/// Handle `DatingScreen::search-requested` from the typed Slint callback.
pub fn dating_search_requested(ctx: &AppContext, filters: crate::DatingSearchFilters) -> bool {
    let Some(_account_id) = account_id(ctx) else {
        return false;
    };
    let Some(filters) = discover_filters_from_draft(&filters) else {
        ctx.apply_ui(|window| {
            window.set_dating_search_error("Проверьте фильтры поиска.".into());
        });
        return true;
    };
    if let Ok(mut state) = state().lock() {
        state.dating_filters = filters.clone();
        state.dating_results.clear();
        state.dating_cursor = None;
        state.dating_search_started = true;
    }
    load_dating_page(ctx, filters, None, false);
    true
}

fn load_dating_page(
    ctx: &AppContext,
    filters: discovery_dating::DiscoverFilters,
    cursor: Option<discovery_dating::DiscoverCursor>,
    append: bool,
) {
    let query = discovery_dating::DiscoverQuery {
        filters: filters.clone(),
        cursor,
        limit: Some(DATING_PAGE_SIZE),
    };
    ctx.apply_ui(move |window| {
        window.set_dating_busy(true);
        window.set_dating_loading(true);
        window.set_dating_search_error("".into());
        window.set_dating_search_started(true);
        if !append {
            window.set_dating_results(model(Vec::<DatingCard>::new()));
            window.set_dating_has_more(false);
        }
    });
    let result = ctx.call(
        "friendshub-dating-search",
        move |client| discovery_dating::list_discover_profiles(client, &query),
        move |window, result| match result {
            Ok(page) => {
                let (results, has_more) = if let Ok(mut state) = state().lock() {
                    if !append {
                        state.dating_results.clear();
                    }
                    state.dating_results.extend(page.iter().cloned());
                    state.dating_cursor = if page.len() == DATING_PAGE_SIZE as usize {
                        page.last().map(|card| discovery_dating::DiscoverCursor {
                            before: card.updated_at,
                            before_account_id: card.account_id.clone(),
                        })
                    } else {
                        None
                    };
                    state.dating_search_started = true;
                    (state.dating_results.clone(), state.dating_cursor.is_some())
                } else {
                    (page, false)
                };
                window.set_dating_results(model(
                    results.into_iter().map(dating_card_from_result).collect(),
                ));
                window.set_dating_loading(false);
                window.set_dating_busy(false);
                window.set_dating_search_started(true);
                window.set_dating_has_more(has_more);
                window.set_dating_search_error("".into());
                success(&window);
            }
            Err(error) => {
                window.set_dating_loading(false);
                window.set_dating_busy(false);
                window.set_dating_search_started(true);
                window.set_dating_search_error(error.user_message().into());
                failure(&window, error);
            }
        },
    );
    if result.is_err() {
        ctx.apply_ui(|window| {
            window.set_dating_loading(false);
            window.set_dating_busy(false);
            window.set_dating_search_error("Не удалось запустить поиск.".into());
        });
        report_spawn_failure(ctx);
    }
}

fn handle_channel(ctx: &AppContext, action: &str, arg1: &str, arg2: &str) -> bool {
    let Some(account_id) = account_id(ctx) else {
        return false;
    };
    let Some(window) = ctx.ui.upgrade() else {
        return true;
    };
    let Some(conversation_id) = selected_conversation(&window) else {
        return true;
    };
    match action {
        "refresh" => {
            load_channel(ctx, account_id, conversation_id);
            true
        }
        "publish" => {
            let channel_id = conversation_id.clone();
            let text = arg1.trim().to_owned();
            let editing_post_id = state()
                .lock()
                .ok()
                .and_then(|state| state.channel_editing_post_id.clone());
            ctx.apply_ui(|window| {
                window.set_channel_busy(true);
                window.set_channel_error_text("".into());
            });
            let result = ctx.call(
                "friendshub-channel-publish",
                move |client| {
                    if let Some(post_id) = editing_post_id {
                        community_api::edit_post(client, &post_id, &text)
                    } else {
                        community_api::publish_post(
                            client,
                            &community_api::PublishPost {
                                channel_id: &channel_id,
                                text: &text,
                                attachment_ids: &[],
                                reply_to_post_id: None,
                                forward_from: None,
                            },
                        )
                        .map(|_| ())
                    }
                },
                move |window, result| match result {
                    Ok(()) => {
                        if let Ok(mut state) = state().lock() {
                            state.channel_editing_post_id = None;
                        }
                        window.set_channel_busy(false);
                        window.set_channel_editing(false);
                        window.set_channel_draft("".into());
                        channel_success(&window);
                        window.invoke_ui_action(
                            "channel".into(),
                            "refresh".into(),
                            "".into(),
                            "".into(),
                            "".into(),
                        );
                    }
                    Err(error) => {
                        window.set_channel_busy(false);
                        channel_failure(&window, error);
                    }
                },
            );
            if result.is_err() {
                ctx.apply_ui(|window| {
                    window.set_channel_busy(false);
                    window.set_channel_error_text("Не удалось запустить публикацию.".into());
                });
                report_spawn_failure(ctx);
            }
            true
        }
        "load-older" => {
            let before = state()
                .lock()
                .ok()
                .and_then(|state| state.channel_posts.last().map(|post| post.id.clone()));
            let Some(before) = before else {
                return true;
            };
            ctx.apply_ui(|window| {
                window.set_channel_busy(true);
                window.set_channel_error_text("".into());
            });
            let worker_id = conversation_id.clone();
            let actor_id = account_id.clone();
            let result = ctx.call(
                "friendshub-channel-older-posts",
                move |client| {
                    let page = community_api::channel_posts(
                        client,
                        &worker_id,
                        Some(&before),
                        Some(CHANNEL_POST_PAGE_SIZE),
                    )?;
                    let rows = page
                        .iter()
                        .map(|post| map_channel_post_data(client, post, &actor_id))
                        .collect::<Result<Vec<_>, CoreError>>()?;
                    Ok((page, rows))
                },
                move |window, result| match result {
                    Ok((page, rows)) => {
                        let has_older = page.len() == CHANNEL_POST_PAGE_SIZE as usize;
                        if let Ok(mut state) = state().lock() {
                            state.channel_posts.extend(page);
                            state.channel_has_older = has_older;
                        }
                        let mut all_rows: Vec<UiChannelPost> =
                            window.get_channel_posts().iter().collect();
                        all_rows.extend(rows.into_iter().map(UiChannelPost::from));
                        window.set_channel_posts(model(all_rows));
                        window.set_channel_has_older(has_older);
                        window.set_channel_busy(false);
                        channel_success(&window);
                    }
                    Err(error) => {
                        window.set_channel_busy(false);
                        channel_failure(&window, error);
                    }
                },
            );
            if result.is_err() {
                ctx.apply_ui(|window| {
                    window.set_channel_busy(false);
                    window.set_channel_error_text("Не удалось загрузить публикации.".into());
                });
                report_spawn_failure(ctx);
            }
            true
        }
        "post-delete" | "post-pin" | "post-reaction" | "post-edit" | "post-open"
        | "post-discussion" => {
            if action == "post-open" {
                return false;
            }
            let Some(post) = cached_post(&conversation_id, arg1) else {
                return true;
            };
            match action {
                "post-edit" => {
                    if let Ok(mut state) = state().lock() {
                        state.channel_editing_post_id = Some(post.id);
                    }
                    ctx.apply_ui(move |window| {
                        window.set_channel_draft(post.text.into());
                        window.set_channel_editing(true);
                        window.set_channel_open_post_menu_index(-1);
                        window.set_channel_error_text("".into());
                    });
                }
                "post-delete" => {
                    mutate_channel_post(ctx, conversation_id, post, PostMutation::Delete)
                }
                "post-pin" => mutate_channel_post(
                    ctx,
                    conversation_id,
                    post.clone(),
                    if post.pinned_at.is_some() {
                        PostMutation::Unpin
                    } else {
                        PostMutation::Pin
                    },
                ),
                "post-reaction" => mutate_channel_post(
                    ctx,
                    conversation_id,
                    post,
                    PostMutation::Reaction {
                        emoji: arg2.to_owned(),
                        account_id: account_id.clone(),
                    },
                ),
                "post-discussion" => {
                    let post_id = post.id;
                    let result = ctx.call(
                        "friendshub-channel-discussion",
                        move |client| community_api::post_discussion(client, &post_id),
                        move |window, result| match result {
                            Ok(discussion) => {
                                success(&window);
                                if let Some(group_id) = discussion.linked_group_id {
                                    window.invoke_navigate_requested(
                                        format!("/app/chat/{group_id}").into(),
                                    );
                                }
                            }
                            Err(error) => failure(&window, error),
                        },
                    );
                    if result.is_err() {
                        report_spawn_failure(ctx);
                    }
                }
                _ => {}
            }
            true
        }
        "draft" => true,
        "cancel-edit" => {
            if let Ok(mut state) = state().lock() {
                state.channel_editing_post_id = None;
            }
            ctx.apply_ui(|window| {
                window.set_channel_draft("".into());
                window.set_channel_editing(false);
                window.set_channel_error_text("".into());
            });
            true
        }
        _ => false,
    }
}

enum PostMutation {
    Delete,
    Pin,
    Unpin,
    Reaction { emoji: String, account_id: String },
}

fn cached_post(conversation_id: &str, index: &str) -> Option<community_api::ChannelPost> {
    let index = index.parse::<usize>().ok()?;
    let state = state().lock().ok()?;
    state
        .channel_posts
        .get(index)
        .filter(|post| post.channel_id == conversation_id)
        .cloned()
}

fn mutate_channel_post(
    ctx: &AppContext,
    conversation_id: String,
    post: community_api::ChannelPost,
    mutation: PostMutation,
) {
    let _ = conversation_id;
    ctx.apply_ui(|window| {
        window.set_channel_busy(true);
        window.set_channel_error_text("".into());
    });
    let result = ctx.call(
        "friendshub-channel-post-mutation",
        move |client| {
            match mutation {
                PostMutation::Delete => community_api::delete_post(client, &post.id)?,
                PostMutation::Pin => community_api::pin_post(client, &post.id)?,
                PostMutation::Unpin => community_api::unpin_post(client, &post.id)?,
                PostMutation::Reaction { emoji, account_id } => {
                    let current =
                        all_post_reactions(client, &post.id)?
                            .into_iter()
                            .any(|reaction| {
                                reaction.actor_type == "account"
                                    && reaction.actor_id == account_id
                                    && reaction.emoji == emoji
                            });
                    if current {
                        community_api::remove_post_reaction(client, &post.id)?;
                    } else {
                        community_api::set_post_reaction(client, &post.id, &emoji)?;
                    }
                }
            }
            Ok(())
        },
        move |window, result| match result {
            Ok(()) => {
                window.set_channel_busy(false);
                channel_success(&window);
                window.invoke_ui_action(
                    "channel".into(),
                    "refresh".into(),
                    "".into(),
                    "".into(),
                    "".into(),
                );
            }
            Err(error) => {
                window.set_channel_busy(false);
                channel_failure(&window, error);
            }
        },
    );
    if result.is_err() {
        ctx.apply_ui(|window| {
            window.set_channel_busy(false);
            window.set_channel_error_text("Не удалось запустить действие с публикацией.".into());
        });
        report_spawn_failure(ctx);
    }
}

fn handle_conversation_settings(
    ctx: &AppContext,
    action: &str,
    arg1: &str,
    arg2: &str,
    arg3: &str,
) -> bool {
    let Some(_account_id) = account_id(ctx) else {
        return false;
    };
    let Some(window) = ctx.ui.upgrade() else {
        return true;
    };
    let Some(conversation_id) = selected_conversation(&window) else {
        return true;
    };
    let is_channel = window.get_conversation_kind().as_str() == "channel";
    match action {
        "retry" => {
            let route = window.get_route().to_string();
            load(ctx, &route);
            true
        }
        "save-metadata" => {
            if is_channel {
                return false;
            }
            let conversation_id = conversation_id.clone();
            let title = arg1.trim().to_owned();
            let description = arg2.trim().to_owned();
            let is_public = arg3 == "true";
            settings_mutation(ctx, move |client| {
                community_api::update_group(
                    client,
                    &community_api::GroupPatch {
                        conversation_id: &conversation_id,
                        title: Some(&title),
                        description: Some(&description),
                        is_public: Some(is_public),
                    },
                )
            });
            true
        }
        "save-handle" => {
            let id = conversation_id.clone();
            let handle = normalize_handle(arg1);
            settings_mutation(ctx, move |client| {
                if is_channel {
                    community_api::set_channel_handle(client, &id, &handle).map(|_| ())
                } else {
                    community_api::set_group_handle(client, &id, &handle).map(|_| ())
                }
            });
            true
        }
        "set-public" => {
            let id = conversation_id.clone();
            let is_public = arg1 == "true";
            settings_mutation(ctx, move |client| {
                if is_channel {
                    community_api::set_channel_public(client, &id, is_public)
                } else {
                    community_api::update_group(
                        client,
                        &community_api::GroupPatch {
                            conversation_id: &id,
                            title: None,
                            description: None,
                            is_public: Some(is_public),
                        },
                    )
                }
            });
            true
        }
        "link-group" if is_channel => {
            let id = conversation_id.clone();
            let linked_group_id = (!arg1.is_empty()).then(|| arg1.to_owned());
            settings_mutation(ctx, move |client| {
                community_api::set_linked_group(client, &id, linked_group_id.as_deref())
            });
            true
        }
        "add-member" if !is_channel => {
            let id = conversation_id.clone();
            let account_to_add = arg1.to_owned();
            settings_mutation(ctx, move |client| {
                community_api::add_group_member(client, &id, &account_to_add)
            });
            true
        }
        "member-action" if !is_channel => {
            let Some(member) = cached_group_member(arg1) else {
                return true;
            };
            match arg2 {
                "remove" => {
                    let id = conversation_id.clone();
                    settings_mutation(ctx, move |client| {
                        community_api::remove_group_member(client, &id, &member.account_id)
                    });
                    true
                }
                "unmute" => {
                    let id = conversation_id.clone();
                    settings_mutation(ctx, move |client| {
                        community_api::mute_group_member(client, &id, &member.account_id, None)
                    });
                    true
                }
                // WEB presents a rights editor, mute-duration chooser, and tag
                // editor. The current native callback carries no values from
                // those dialogs, so choosing defaults here would be unsafe.
                _ => false,
            }
        }
        "member-action" if is_channel => {
            let index = match arg1.parse::<usize>() {
                Ok(index) => index,
                Err(_) => return true,
            };
            let Some(member) = state()
                .lock()
                .ok()
                .and_then(|state| state.channel_members.get(index).cloned())
            else {
                return true;
            };
            let id = conversation_id.clone();
            match arg2 {
                "remove" => {
                    settings_mutation(ctx, move |client| {
                        community_api::remove_channel_member(
                            client,
                            &id,
                            &member.actor_type,
                            &member.actor_id,
                        )
                    });
                    true
                }
                "make_admin" => {
                    settings_mutation(ctx, move |client| {
                        community_api::set_channel_member_role(
                            client,
                            &id,
                            &member.actor_type,
                            &member.actor_id,
                            "admin",
                        )
                    });
                    true
                }
                _ => false,
            }
        }
        "banned-member-action" if !is_channel && arg2 == "unban" => {
            let Some(account_to_unban) = cached_group_ban(arg1) else {
                return true;
            };
            let id = conversation_id.clone();
            settings_mutation(ctx, move |client| {
                community_api::unban_group_member(client, &id, &account_to_unban)
            });
            true
        }
        "owner-transfer" => {
            let index = arg1.parse::<usize>().ok();
            let target = if is_channel {
                index.and_then(cached_channel_owner)
            } else {
                index.and_then(cached_group_owner)
            };
            let Some(new_owner) = target else {
                return true;
            };
            let id = conversation_id.clone();
            settings_mutation(ctx, move |client| {
                if is_channel {
                    community_api::transfer_channel_owner(client, &id, &new_owner)
                } else {
                    community_api::transfer_group_owner(client, &id, &new_owner)
                }
            });
            true
        }
        "analytics-refresh" if is_channel => {
            ctx.apply_ui(|window| {
                window.set_conversation_settings_busy(true);
                window.set_conversation_settings_error_text("".into());
            });
            let id = conversation_id.clone();
            let result = ctx.call(
                "friendshub-channel-analytics",
                move |client| community_api::channel_analytics(client, &id),
                move |window, result| match result {
                    Ok(analytics) => {
                        let post_snapshots = state()
                            .lock()
                            .map(|state| state.channel_posts.clone())
                            .unwrap_or_default();
                        let rows = model(analytics_rows(&analytics.posts, &post_snapshots));
                        if let Ok(mut state) = state().lock() {
                            state.channel_analytics = analytics.posts;
                        }
                        window.set_conversation_settings_analytics(rows);
                        window.set_conversation_settings_busy(false);
                        success(&window);
                    }
                    Err(error) => {
                        window.set_conversation_settings_busy(false);
                        window.set_conversation_settings_error_text(error.user_message().into());
                        failure(&window, error);
                    }
                },
            );
            if result.is_err() {
                ctx.apply_ui(|window| {
                    window.set_conversation_settings_busy(false);
                    window.set_conversation_settings_error_text(
                        "Не удалось запустить обновление статистики.".into(),
                    );
                });
                report_spawn_failure(ctx);
            }
            true
        }
        "delete" => {
            if let Ok(mut state) = state().lock() {
                state.pending_delete = Some((conversation_id, is_channel));
            }
            ctx.apply_ui(move |window| {
                window.set_conversation_settings_confirmation_title(
                    if is_channel {
                        "Удалить канал?"
                    } else {
                        "Удалить группу?"
                    }
                    .into(),
                );
                window.set_conversation_settings_confirmation_description(
                    if is_channel {
                        "Канал будет удалён вместе с его публикациями."
                    } else {
                        "Группа будет удалена вместе с её участниками."
                    }
                    .into(),
                );
                window.set_conversation_settings_confirmation_visible(true);
            });
            true
        }
        "confirm" => {
            let pending = state()
                .lock()
                .ok()
                .and_then(|mut state| state.pending_delete.take());
            let Some((id, delete_channel)) = pending else {
                return true;
            };
            ctx.apply_ui(|window| {
                window.set_conversation_settings_confirmation_visible(false);
                window.set_conversation_settings_busy(true);
                window.set_conversation_settings_error_text("".into());
            });
            let result = ctx.call(
                "friendshub-community-delete",
                move |client| {
                    if delete_channel {
                        community_api::delete_channel(client, &id)
                    } else {
                        community_api::delete_group(client, &id)
                    }
                },
                move |window, result| match result {
                    Ok(()) => {
                        window.set_conversation_settings_busy(false);
                        success(&window);
                        window.invoke_navigate_requested("/app".into());
                    }
                    Err(error) => {
                        window.set_conversation_settings_busy(false);
                        window.set_conversation_settings_error_text(error.user_message().into());
                        failure(&window, error);
                    }
                },
            );
            if result.is_err() {
                ctx.apply_ui(|window| {
                    window.set_conversation_settings_busy(false);
                    window.set_conversation_settings_error_text(
                        "Не удалось запустить удаление беседы.".into(),
                    );
                });
                report_spawn_failure(ctx);
            }
            true
        }
        "cancel-confirmation" => {
            if let Ok(mut state) = state().lock() {
                state.pending_delete = None;
            }
            ctx.apply_ui(|window| {
                window.set_conversation_settings_confirmation_visible(false);
            });
            true
        }
        "photo-remove" => {
            let id = conversation_id.clone();
            settings_mutation(ctx, move |client| {
                if is_channel {
                    community_api::delete_channel_avatar(client, &id)
                } else {
                    community_api::delete_group_avatar(client, &id)
                }
            });
            true
        }
        "channel-bot-add" if is_channel => {
            let id = conversation_id.clone();
            let bot_id = arg1.to_owned();
            settings_mutation(ctx, move |client| {
                community_api::add_channel_bot(client, &id, &bot_id)
            });
            true
        }
        "photo-change" | "member-action" | "banned-member-action" | "channel-bot-add" => false,
        _ => false,
    }
}

fn settings_mutation(
    ctx: &AppContext,
    call: impl FnOnce(&Client) -> Result<(), CoreError> + Send + 'static,
) {
    ctx.apply_ui(|window| {
        window.set_conversation_settings_busy(true);
        window.set_conversation_settings_error_text("".into());
        window.set_conversation_settings_profile_error_text("".into());
    });
    let result = ctx.call(
        "friendshub-community-settings-mutation",
        call,
        move |window, result| match result {
            Ok(()) => {
                window.set_conversation_settings_busy(false);
                success(&window);
                window.invoke_ui_action(
                    "conversation-settings".into(),
                    "retry".into(),
                    "".into(),
                    "".into(),
                    "".into(),
                );
            }
            Err(error) => {
                window.set_conversation_settings_busy(false);
                window.set_conversation_settings_error_text(error.user_message().into());
                failure(&window, error);
            }
        },
    );
    if result.is_err() {
        ctx.apply_ui(|window| {
            window.set_conversation_settings_busy(false);
            window.set_conversation_settings_error_text(
                "Не удалось запустить изменение настроек.".into(),
            );
        });
        report_spawn_failure(ctx);
    }
}

fn cached_group_member(index: &str) -> Option<community_api::GroupMember> {
    state()
        .lock()
        .ok()?
        .group_members
        .get(index.parse::<usize>().ok()?)
        .cloned()
}

fn cached_group_ban(index: &str) -> Option<String> {
    state()
        .lock()
        .ok()?
        .group_bans
        .get(index.parse::<usize>().ok()?)
        .map(|ban| ban.account_id.clone())
}

fn cached_group_owner(index: usize) -> Option<String> {
    state()
        .lock()
        .ok()?
        .group_members
        .get(index)
        .map(|member| member.account_id.clone())
}

fn cached_channel_owner(index: usize) -> Option<String> {
    state()
        .lock()
        .ok()?
        .channel_members
        .get(index)
        .filter(|member| member.actor_type == "account")
        .map(|member| member.actor_id.clone())
}

fn handle_invites(ctx: &AppContext, action: &str, arg1: &str) -> bool {
    let Some(_account_id) = account_id(ctx) else {
        return false;
    };
    match action {
        "create" => {
            let Some(window) = ctx.ui.upgrade() else {
                return true;
            };
            let Some(conversation_id) = selected_conversation(&window) else {
                return true;
            };
            let id = conversation_id.clone();
            let kind = arg1.to_owned();
            ctx.apply_ui(|window| {
                window.set_conversation_settings_busy(true);
                window.set_conversation_settings_error_text("".into());
            });
            let result = ctx.call(
                "friendshub-invite-create",
                move |client| {
                    community_api::create_invite(client, &id, &kind)?;
                    community_api::invites(client, &id)
                },
                move |window, result| match result {
                    Ok(invites) => {
                        let rows = model(invite_rows(&invites));
                        if let Ok(mut state) = state().lock() {
                            state.invites = invites;
                        }
                        window.set_conversation_settings_invites(rows);
                        window.set_conversation_settings_busy(false);
                        success(&window);
                    }
                    Err(error) => {
                        window.set_conversation_settings_busy(false);
                        window.set_conversation_settings_error_text(error.user_message().into());
                        failure(&window, error);
                    }
                },
            );
            if result.is_err() {
                ctx.apply_ui(|window| {
                    window.set_conversation_settings_busy(false);
                    window.set_conversation_settings_error_text(
                        "Не удалось запустить создание ссылки.".into(),
                    );
                });
                report_spawn_failure(ctx);
            }
            true
        }
        "revoke" => {
            let Some(window) = ctx.ui.upgrade() else {
                return true;
            };
            let Some(conversation_id) = selected_conversation(&window) else {
                return true;
            };
            let invite_id = state().lock().ok().and_then(|state| {
                state
                    .invites
                    .get(arg1.parse::<usize>().ok()?)
                    .map(|invite| invite.id.clone())
            });
            let Some(invite_id) = invite_id else {
                return true;
            };
            let conversation_id_for_list = conversation_id.clone();
            ctx.apply_ui(|window| {
                window.set_conversation_settings_busy(true);
                window.set_conversation_settings_error_text("".into());
            });
            let result = ctx.call(
                "friendshub-invite-revoke",
                move |client| {
                    community_api::revoke_invite(client, &invite_id)?;
                    community_api::invites(client, &conversation_id_for_list)
                },
                move |window, result| match result {
                    Ok(invites) => {
                        let rows = model(invite_rows(&invites));
                        if let Ok(mut state) = state().lock() {
                            state.invites = invites;
                        }
                        window.set_conversation_settings_invites(rows);
                        window.set_conversation_settings_busy(false);
                        success(&window);
                    }
                    Err(error) => {
                        window.set_conversation_settings_busy(false);
                        window.set_conversation_settings_error_text(error.user_message().into());
                        failure(&window, error);
                    }
                },
            );
            if result.is_err() {
                ctx.apply_ui(|window| {
                    window.set_conversation_settings_busy(false);
                    window.set_conversation_settings_error_text(
                        "Не удалось запустить отзыв ссылки.".into(),
                    );
                });
                report_spawn_failure(ctx);
            }
            true
        }
        "join" | "retry" => {
            let token = arg1.to_owned();
            let result = ctx.call(
                "friendshub-invite-join",
                move |client| community_api::join_invite(client, &token),
                move |window, result| match result {
                    Ok(joined) => {
                        success(&window);
                        window.invoke_navigate_requested(
                            format!("/app/chat/{}", joined.conversation_id).into(),
                        );
                    }
                    Err(error) => failure(&window, error),
                },
            );
            if result.is_err() {
                report_spawn_failure(ctx);
            }
            true
        }
        // Clipboard access is a platform operation; this controller has no
        // clipboard capability and must not claim that copying succeeded.
        "copy" => false,
        _ => false,
    }
}

/// Handler for the typed `MainWindow::create_group_requested` callback.
pub fn create_group_requested(
    ctx: &AppContext,
    title: SharedString,
    description: SharedString,
    handle: SharedString,
    is_public: bool,
    selected_contacts: ModelRc<GroupMemberRow>,
) -> bool {
    let Some(_account_id) = account_id(ctx) else {
        return false;
    };
    let member_ids = selected_contacts
        .iter()
        .filter(|row| row.selected)
        .map(|row| row.account_id.to_string())
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    let title = title.trim().to_owned();
    if title.is_empty() {
        return true;
    }
    let description = description.trim().to_owned();
    let handle = normalize_handle(&handle);
    ctx.apply_ui(|window| {
        window.set_create_group_busy(true);
        window.set_create_group_handle_feedback("".into());
        window.set_create_group_handle_feedback_error(false);
    });
    let result = ctx.call(
        "friendshub-group-create",
        move |client| {
            let created = community_api::create_group(
                client,
                &community_api::CreateGroup {
                    title: &title,
                    member_ids: &member_ids,
                    is_public,
                    description: (!description.is_empty()).then_some(description.as_str()),
                    expires_in_seconds: None,
                },
            )?;
            let handle_set = handle.is_empty()
                || community_api::set_group_handle(client, &created.conversation_id, &handle)
                    .is_ok();
            Ok((created, handle_set))
        },
        move |window, result| match result {
            Ok((created, handle_set)) => {
                window.set_create_group_busy(false);
                window.set_selected_conversation_id(created.conversation_id.clone().into());
                window.set_conversation_kind("group".into());
                window.set_conversation_title(
                    created.title.unwrap_or_else(|| "Группа".to_owned()).into(),
                );
                window.invoke_navigate_requested(
                    format!("/app/chat/{}", created.conversation_id).into(),
                );
                if handle_set {
                    success(&window);
                } else {
                    window.set_conversations_error(
                        "Группа создана, но адрес не удалось сохранить.".into(),
                    );
                }
            }
            Err(error) => {
                window.set_create_group_busy(false);
                failure(&window, error);
            }
        },
    );
    if result.is_err() {
        ctx.apply_ui(|window| {
            window.set_create_group_busy(false);
        });
        report_spawn_failure(ctx);
    }
    true
}

/// Handler for the typed `MainWindow::create_channel_requested` callback.
pub fn create_channel_requested(
    ctx: &AppContext,
    title: SharedString,
    description: SharedString,
    handle: SharedString,
    is_public: bool,
    discussion_kind: SharedString,
    discussion_title: SharedString,
    discussion_handle: SharedString,
    selected_group_id: SharedString,
) -> bool {
    let Some(_account_id) = account_id(ctx) else {
        return false;
    };
    let title = title.trim().to_owned();
    if title.is_empty() {
        return true;
    }
    let description = description.trim().to_owned();
    let handle = normalize_handle(&handle);
    let discussion_kind = discussion_kind.to_string();
    let discussion_title = discussion_title.trim().to_owned();
    let discussion_handle = normalize_handle(&discussion_handle);
    let selected_group_id = selected_group_id.to_string();
    ctx.apply_ui(|window| {
        window.set_create_channel_busy(true);
        window.set_create_channel_handle_feedback("".into());
        window.set_create_channel_handle_feedback_error(false);
        window.set_create_channel_discussion_handle_feedback("".into());
        window.set_create_channel_discussion_handle_feedback_error(false);
    });
    let result = ctx.call(
        "friendshub-channel-create",
        move |client| {
            let created = community_api::create_channel(
                client,
                &community_api::CreateChannel {
                    title: &title,
                    description: (!description.is_empty()).then_some(description.as_str()),
                    is_public,
                },
            )?;
            let mut follow_up_ok = handle.is_empty()
                || community_api::set_channel_handle(client, &created.conversation_id, &handle)
                    .is_ok();
            let link_result = match discussion_kind.as_str() {
                "existing" if !selected_group_id.is_empty() => community_api::set_linked_group(
                    client,
                    &created.conversation_id,
                    Some(&selected_group_id),
                ),
                "new" if !discussion_title.is_empty() => {
                    let group = community_api::create_group(
                        client,
                        &community_api::CreateGroup {
                            title: &discussion_title,
                            member_ids: &[],
                            is_public: false,
                            description: None,
                            expires_in_seconds: None,
                        },
                    );
                    match group {
                        Ok(group) => {
                            if !discussion_handle.is_empty()
                                && community_api::set_group_handle(
                                    client,
                                    &group.conversation_id,
                                    &discussion_handle,
                                )
                                .is_err()
                            {
                                follow_up_ok = false;
                            }
                            community_api::set_linked_group(
                                client,
                                &created.conversation_id,
                                Some(&group.conversation_id),
                            )
                        }
                        Err(error) => Err(error),
                    }
                }
                "none" | "" => Ok(()),
                _ => {
                    follow_up_ok = false;
                    Ok(())
                }
            };
            if link_result.is_err() {
                follow_up_ok = false;
            }
            Ok((created, follow_up_ok))
        },
        move |window, result| match result {
            Ok((created, follow_up_ok)) => {
                window.set_create_channel_busy(false);
                window.set_selected_conversation_id(created.conversation_id.clone().into());
                window.set_conversation_kind("channel".into());
                window.set_conversation_title(
                    created.title.unwrap_or_else(|| "Канал".to_owned()).into(),
                );
                window.invoke_navigate_requested(
                    format!("/app/channel/{}", created.conversation_id).into(),
                );
                if follow_up_ok {
                    success(&window);
                } else {
                    window.set_conversations_error(
                        "Канал создан, но адрес или группа обсуждения настроены не полностью."
                            .into(),
                    );
                }
            }
            Err(error) => {
                window.set_create_channel_busy(false);
                failure(&window, error);
            }
        },
    );
    if result.is_err() {
        ctx.apply_ui(|window| {
            window.set_create_channel_busy(false);
        });
        report_spawn_failure(ctx);
    }
    true
}

fn profile_to_draft(profile: &discovery_dating::DiscoverProfile) -> DatingProfileDraft {
    DatingProfileDraft {
        display_name: profile.display_name.clone().into(),
        city: profile.city.clone().into(),
        birth_year: profile.birth_year.to_string().into(),
        gender: gender_name(profile.gender).into(),
        looking_for: looking_for_name(profile.looking_for).into(),
        bio: profile.bio.clone().unwrap_or_default().into(),
        interests: profile.interests.join(", ").into(),
        is_listed: profile.is_listed,
    }
}

fn empty_profile_draft() -> DatingProfileDraft {
    DatingProfileDraft {
        display_name: "".into(),
        city: "".into(),
        birth_year: "".into(),
        gender: "unspecified".into(),
        looking_for: "friendship".into(),
        bio: "".into(),
        interests: "".into(),
        is_listed: true,
    }
}

fn profile_input_from_draft(
    draft: &DatingProfileDraft,
) -> Option<discovery_dating::DiscoverProfileInput> {
    let birth_year = draft.birth_year.trim().parse::<i32>().ok()?;
    let gender = parse_gender(&draft.gender)?;
    let looking_for = parse_looking_for(&draft.looking_for)?;
    Some(discovery_dating::DiscoverProfileInput {
        display_name: draft.display_name.trim().to_owned(),
        city: draft.city.trim().to_owned(),
        birth_year,
        gender,
        looking_for,
        bio: (!draft.bio.trim().is_empty()).then(|| draft.bio.trim().to_owned()),
        interests: parse_tags(&draft.interests),
        is_listed: draft.is_listed,
    })
}

fn profile_to_input(
    profile: &discovery_dating::DiscoverProfile,
    is_listed: bool,
) -> discovery_dating::DiscoverProfileInput {
    discovery_dating::DiscoverProfileInput {
        display_name: profile.display_name.clone(),
        city: profile.city.clone(),
        birth_year: profile.birth_year,
        gender: profile.gender,
        looking_for: profile.looking_for,
        bio: profile.bio.clone(),
        interests: profile.interests.clone(),
        is_listed,
    }
}

fn discover_filters_from_draft(
    draft: &crate::DatingSearchFilters,
) -> Option<discovery_dating::DiscoverFilters> {
    let gender = match draft.gender.as_str() {
        "" | "any" => None,
        "male" => Some(discovery_dating::DiscoverGenderFilter::Male),
        "female" => Some(discovery_dating::DiscoverGenderFilter::Female),
        "other" => Some(discovery_dating::DiscoverGenderFilter::Other),
        _ => return None,
    };
    let looking_for = match draft.looking_for.as_str() {
        "" | "any" => None,
        value => Some(parse_looking_for(value)?),
    };
    Some(discovery_dating::DiscoverFilters {
        city: nonempty(draft.city.as_str()),
        gender,
        looking_for,
        min_age: parse_optional_i32(&draft.min_age)?,
        max_age: parse_optional_i32(&draft.max_age)?,
        interests: {
            let tags = parse_tags(&draft.interests);
            (!tags.is_empty()).then_some(tags)
        },
        query: nonempty(draft.query.as_str()),
    })
}

fn parse_optional_i32(value: &SharedString) -> Option<Option<i32>> {
    if value.trim().is_empty() {
        Some(None)
    } else {
        value.trim().parse::<i32>().ok().map(Some)
    }
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_tags(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_gender(value: &str) -> Option<discovery_dating::DiscoverGender> {
    match value {
        "male" => Some(discovery_dating::DiscoverGender::Male),
        "female" => Some(discovery_dating::DiscoverGender::Female),
        "other" => Some(discovery_dating::DiscoverGender::Other),
        "unspecified" => Some(discovery_dating::DiscoverGender::Unspecified),
        _ => None,
    }
}

fn parse_looking_for(value: &str) -> Option<discovery_dating::DiscoverLookingFor> {
    match value {
        "friendship" => Some(discovery_dating::DiscoverLookingFor::Friendship),
        "relationship" => Some(discovery_dating::DiscoverLookingFor::Relationship),
        "chat" => Some(discovery_dating::DiscoverLookingFor::Chat),
        "all" => Some(discovery_dating::DiscoverLookingFor::All),
        _ => None,
    }
}

fn gender_name(value: discovery_dating::DiscoverGender) -> &'static str {
    match value {
        discovery_dating::DiscoverGender::Male => "male",
        discovery_dating::DiscoverGender::Female => "female",
        discovery_dating::DiscoverGender::Other => "other",
        discovery_dating::DiscoverGender::Unspecified => "unspecified",
    }
}

fn looking_for_name(value: discovery_dating::DiscoverLookingFor) -> &'static str {
    match value {
        discovery_dating::DiscoverLookingFor::Friendship => "friendship",
        discovery_dating::DiscoverLookingFor::Relationship => "relationship",
        discovery_dating::DiscoverLookingFor::Chat => "chat",
        discovery_dating::DiscoverLookingFor::All => "all",
    }
}

fn normalize_handle(value: &str) -> String {
    value.trim().trim_start_matches('@').to_ascii_lowercase()
}

fn initials(value: &str) -> String {
    value
        .split_whitespace()
        .take(2)
        .filter_map(|part| part.chars().next())
        .collect::<String>()
        .to_uppercase()
}

fn format_timestamp(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        seconds / 3_600,
        (seconds % 3_600) / 60
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
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
