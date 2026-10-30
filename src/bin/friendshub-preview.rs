//! Headless preview renderer for the FriendsHub Slint UI.
//!
//! Renders one screen with representative sample data into a PNG without a
//! display server. Used for visual review of the native UI against the web
//! app; it is not part of any shipped platform binary.
//!
//! Usage:
//!     cargo run --bin friendshub-preview -- --screen chat --out /tmp/chat.png
//!
//! Options:
//!     --screen <name>   Screen to render (default: login)
//!     --out <path>      Output PNG path (default: preview.png)
//!     --width <px>      Physical width (default: 1120)
//!     --height <px>     Physical height (default: 760)
//!     --light           Render the light theme
//!     --lang <ru|en>    Interface language (default: ru)

use std::error::Error;
use std::rc::Rc;

use friendshub_apps::{
    BotMessage, CallHistoryRow, ChannelPost, ChatAttachmentView, ChatMessage, ChatReaction,
    ConnectStepState, ConversationMember, ConversationRow, DatingCard, DatingProfileDraft,
    DatingSearchFilters, DiscoverEntry, FilterTab, FolderTab, GroupMemberRow, MainWindow,
    OwnProfileField, PollOptionView, PollViewModel, PublicProfileField, SecurityDevice,
    SecurityEvent, SecuritySession, SettingsAccount, SettingsBot, SettingsContact, SettingsOption,
    SettingsPrivacyException, SettingsProfileField,
};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, Image, ModelRc, PhysicalSize, SharedString, VecModel};

struct PreviewPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for PreviewPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }
}

struct Options {
    screen: String,
    out: String,
    width: u32,
    height: u32,
    light: bool,
    language: String,
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut options = Options {
        screen: "login".into(),
        out: "preview.png".into(),
        width: 1120,
        height: 760,
        light: false,
        language: "ru".into(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--screen" => options.screen = next_value(&mut args, "--screen")?,
            "--out" => options.out = next_value(&mut args, "--out")?,
            "--lang" => options.language = next_value(&mut args, "--lang")?,
            "--width" => options.width = next_value(&mut args, "--width")?.parse()?,
            "--height" => options.height = next_value(&mut args, "--height")?.parse()?,
            "--light" => options.light = true,
            "--help" | "-h" => {
                println!(
                    "friendshub-preview --screen <name> --out <path> [--width N] [--height N] [--light] [--lang ru|en]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(options)
}

fn next_value(
    args: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("{name} requires a value").into())
}

fn text(value: &str) -> SharedString {
    SharedString::from(value)
}

fn model<T: Clone + 'static>(values: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(values))
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PreviewPlatform {
        window: window.clone(),
    }))
    .map_err(|_| "the preview platform was already initialized")?;
    window.set_size(PhysicalSize::new(options.width, options.height));

    let ui = MainWindow::new()?;
    ui.set_ui_font_family("Noto Sans".into());
    populate(&ui, &options);

    ui.show()?;
    // One layout pass is enough for static content: the software renderer
    // resolves the tree when the snapshot is taken, and timers only drive
    // animations that a still image does not need.
    slint::platform::update_timers_and_animations();

    let snapshot = ui.window().take_snapshot()?;
    let image = image::RgbaImage::from_raw(
        snapshot.width(),
        snapshot.height(),
        snapshot.as_bytes().to_vec(),
    )
    .ok_or("snapshot buffer had an unexpected size")?;
    image.save(&options.out)?;
    println!(
        "rendered {} at {}x{} to {}",
        options.screen, options.width, options.height, options.out
    );
    Ok(())
}

fn populate(ui: &MainWindow, options: &Options) {
    ui.set_language(text(&options.language));
    ui.set_light_mode(options.light);

    let screen = options.screen.as_str();
    let authenticated = !matches!(
        screen,
        "login"
            | "register"
            | "two_factor"
            | "connect"
            | "invite"
            | "not_found"
            | "settings_accounts_public"
    );
    ui.set_authenticated(authenticated);
    ui.set_screen(text(screen));

    ui.set_account_username(text("Som_Somovich"));
    ui.set_account_fh_number(text("FH25822288"));
    ui.set_account_avatar_label(text("SS"));
    ui.set_has_account_avatar(false);
    ui.set_connect_username(text("Som_Somovich"));

    ui.set_conversations(model(conversations()));
    ui.set_conversation_filters(model(filters()));
    ui.set_folders(model(vec![FolderTab {
        id: text("team"),
        name: text("Команда"),
        unread_count: 3,
    }]));
    ui.set_selected_filter(text("all"));
    ui.set_selected_conversation_id(text("dm-alice"));
    ui.set_has_archived(true);

    ui.set_conversation_title(text("Алиса Соколова"));
    ui.set_conversation_avatar_label(text("АС"));
    ui.set_conversation_subtitle(text("в сети"));
    ui.set_conversation_kind(text("direct"));
    ui.set_messages(model(messages()));
    ui.set_polls(model(vec![poll()]));
    ui.set_chat_can_call(true);
    ui.set_chat_can_video_call(true);
    ui.set_chat_pin_count(2);
    ui.set_chat_pinned_text(text("Созвон в 19:00, не опаздывай"));
    ui.set_chat_pin_counter(text("1/2"));

    ui.set_channel_posts(model(channel_posts()));
    ui.set_pinned_channel_posts(model(vec![channel_posts()[0].clone()]));
    ui.set_channel_can_publish(true);
    ui.set_conversation_settings_title(text("Настройки группы"));
    ui.set_conversation_settings_description(text("Обсуждаем проект и делимся новостями."));
    ui.set_conversation_settings_members(model(vec![
        ConversationMember {
            actor_type: text("user"),
            can_moderate: true,
            id: text("me"),
            muted: false,
            name: text("Som_Somovich"),
            role: text("Владелец"),
            tag: text("owner"),
        },
        ConversationMember {
            actor_type: text("user"),
            can_moderate: true,
            id: text("alice"),
            muted: false,
            name: text("Алиса Соколова"),
            role: text("Модератор"),
            tag: text("mod"),
        },
        ConversationMember {
            actor_type: text("user"),
            can_moderate: false,
            id: text("bob"),
            muted: true,
            name: text("Боб"),
            role: text("Участник"),
            tag: text(""),
        },
    ]));
    ui.set_conversation_settings_can_edit_profile(true);
    ui.set_conversation_settings_can_manage_members(true);
    ui.set_conversation_settings_is_owner(true);

    ui.set_bot_messages(model(vec![
        BotMessage {
            deleted: false,
            from_bot: true,
            has_reply_keyboard: false,
            id: text("b1"),
            reply_keyboard_labels: text(""),
            text: text("Привет! Я помогу с расписанием и напомню о встречах."),
            time_label: text("12:04"),
        },
        BotMessage {
            deleted: false,
            from_bot: false,
            has_reply_keyboard: false,
            id: text("b2"),
            reply_keyboard_labels: text(""),
            text: text("Напомни мне завтра в 10:00 про созвон."),
            time_label: text("12:05"),
        },
        BotMessage {
            deleted: false,
            from_bot: true,
            has_reply_keyboard: true,
            id: text("b3"),
            reply_keyboard_labels: text("Да, напомни"),
            text: text("Готово. Напомню завтра в 10:00."),
            time_label: text("12:05"),
        },
    ]));

    ui.set_discover_entries(model(vec![
        DiscoverEntry {
            avatar_image: Image::default(),
            conversation_id: text("d1"),
            count_label: text("128 участников"),
            description: text("Сообщество разработчиков и дизайнеров."),
            has_avatar: false,
            initials: text("FH"),
            title: text("FriendsHub Team"),
        },
        DiscoverEntry {
            avatar_image: Image::default(),
            conversation_id: text("d2"),
            count_label: text("42 участника"),
            description: text("Место для обсуждения дизайна интерфейсов."),
            has_avatar: false,
            initials: text("UI"),
            title: text("Interface Lab"),
        },
        DiscoverEntry {
            avatar_image: Image::default(),
            conversation_id: text("d3"),
            count_label: text("2 415 участников"),
            description: text("Новости, релизы и обсуждения Rust."),
            has_avatar: false,
            initials: text("RS"),
            title: text("Rust Digest"),
        },
    ]));
    ui.set_discover_query(text(""));

    ui.set_dating_has_profile(true);
    ui.set_dating_editing(true);
    ui.set_dating_profile_draft(DatingProfileDraft {
        bio: text("Пишу на Rust, рисую интерфейсы и ищу людей для совместных проектов."),
        birth_year: text("1996"),
        city: text("Москва"),
        display_name: text("Сом"),
        gender: text("male"),
        interests: text("Rust, дизайн, музыка"),
        is_listed: true,
        looking_for: text("friends"),
    });
    ui.set_dating_filters(DatingSearchFilters {
        city: text("Москва"),
        gender: text("any"),
        interests: text(""),
        looking_for: text("any"),
        max_age: text("40"),
        min_age: text("22"),
        query: text(""),
    });
    ui.set_dating_results(model(vec![
        DatingCard {
            account_id: text("p1"),
            age: text("27"),
            avatar_image: Image::default(),
            bio: text("Люблю кофе, код и длинные прогулки."),
            city: text("Москва"),
            display_name: text("Алиса"),
            gender_label: text("Девушка"),
            has_avatar: false,
            initials: text("А"),
            interests_label: text("дизайн, фотография, музыка"),
            looking_for_label: text("Дружба"),
        },
        DatingCard {
            account_id: text("p2"),
            age: text("31"),
            avatar_image: Image::default(),
            bio: text("Собираю винил и пишу пет-проекты по вечерам."),
            city: text("Санкт-Петербург"),
            display_name: text("Максим"),
            gender_label: text("Парень"),
            has_avatar: false,
            initials: text("М"),
            interests_label: text("Rust, музыка, кино"),
            looking_for_label: text("Общение"),
        },
    ]));
    ui.set_dating_search_started(true);

    ui.set_profile_status_emoji(text("🚀"));
    ui.set_profile_status_text(text("Работаю над FriendsHub"));
    ui.set_profile_avatar_initials(text("SS"));
    ui.set_profile_has_avatar(false);
    ui.set_profile_fields(model(vec![
        OwnProfileField {
            label: text("Город"),
            value: text("Москва"),
        },
        OwnProfileField {
            label: text("О себе"),
            value: text("Пишу нативное приложение FriendsHub на Rust и Slint."),
        },
    ]));
    ui.set_user_profile_username(text("Алиса Соколова"));
    ui.set_user_profile_alias(text("alice"));
    ui.set_user_profile_fh_number(text("FH1200456"));
    ui.set_user_profile_status_text(text("в сети"));
    ui.set_user_profile_is_contact(true);
    ui.set_user_profile_avatar_initials(text("АС"));
    ui.set_user_profile_fields(model(vec![
        PublicProfileField {
            label: text("Город"),
            value: text("Москва"),
        },
        PublicProfileField {
            label: text("О себе"),
            value: text("Дизайнер интерфейсов. Люблю кофе и длинные прогулки."),
        },
    ]));

    ui.set_calls(model(calls()));
    ui.set_calls_has_calls(true);
    ui.set_call_peer_name(text("Алиса Соколова"));
    ui.set_call_peer_initial(text("АС"));
    ui.set_call_state(text("incoming"));
    ui.set_call_with_video(true);
    ui.set_call_status(text("Входящий видеозвонок"));

    if screen == "call_incoming" || screen == "call_active" {
        ui.set_screen(text("chat_index"));
        ui.set_call_overlay_visible(true);
        ui.set_call_state(text(if screen == "call_incoming" {
            "incoming"
        } else {
            "active"
        }));
        ui.set_call_status(text(if screen == "call_incoming" {
            "Входящий видеозвонок"
        } else {
            "00:42"
        }));
    }

    ui.set_create_group_title(text("Новая команда"));
    ui.set_create_group_description(text("Обсуждаем проект и планируем спринт."));
    ui.set_create_group_handle(text("new-team"));
    ui.set_create_group_is_public(true);
    ui.set_create_group_can_continue(true);
    ui.set_create_group_can_create(true);
    ui.set_create_group_contacts(model(vec![
        GroupMemberRow {
            account_id: text("alice"),
            fh_number: text("FH1200456"),
            initial: text("А"),
            name: text("Алиса Соколова"),
            selected: true,
        },
        GroupMemberRow {
            account_id: text("bob"),
            fh_number: text("FH7788990"),
            initial: text("Б"),
            name: text("Боб"),
            selected: false,
        },
        GroupMemberRow {
            account_id: text("carol"),
            fh_number: text("FH5566778"),
            initial: text("К"),
            name: text("Кэрол"),
            selected: true,
        },
    ]));
    ui.set_create_group_selected_contacts(model(vec![GroupMemberRow {
        account_id: text("alice"),
        fh_number: text("FH1200456"),
        initial: text("А"),
        name: text("Алиса Соколова"),
        selected: true,
    }]));

    ui.set_create_channel_title(text("Новости продукта"));
    ui.set_create_channel_description(text("Релизы, обновления и заметки команды."));
    ui.set_create_channel_handle(text("product-news"));
    ui.set_create_channel_is_public(true);
    ui.set_create_channel_can_continue(true);
    ui.set_create_channel_can_create(true);
    ui.set_create_channel_discussion_summary(text("Нет"));

    ui.set_settings_account_status_text(text("Работаю над FriendsHub"));
    ui.set_settings_account_status_emoji(text("🚀"));
    ui.set_settings_account_profile_fields(model(vec![
        SettingsProfileField {
            label: text("Город"),
            value: text("Москва"),
        },
        SettingsProfileField {
            label: text("О себе"),
            value: text("Пишу нативное приложение FriendsHub на Rust и Slint."),
        },
    ]));
    ui.set_settings_account_profile_fields_loaded(true);
    ui.set_settings_account_username_valid(true);

    ui.set_privacy_contacts(model(contacts()));
    ui.set_privacy_blocked_accounts(model(vec![SettingsContact {
        id: text("blocked-1"),
        name: text("Неизвестный"),
    }]));
    ui.set_privacy_presence_exceptions(model(vec![SettingsPrivacyException {
        id: text("alice"),
        kind: text("always_visible"),
        name: text("Алиса Соколова"),
    }]));
    ui.set_security_totp_enabled(true);
    ui.set_security_sessions(model(vec![
        SecuritySession {
            id: text("s1"),
            ip_address: text("192.168.1.10"),
            is_current: true,
            signed_in_at: text("Сегодня, 09:12"),
            user_agent: text("Linux · FriendsHub 0.2.0"),
        },
        SecuritySession {
            id: text("s2"),
            ip_address: text("10.0.0.4"),
            is_current: false,
            signed_in_at: text("Вчера, 18:40"),
            user_agent: text("Android · FriendsHub 0.2.0"),
        },
    ]));
    ui.set_security_devices(model(vec![
        SecurityDevice {
            added_at: text("12 октября"),
            can_revoke: true,
            id: text("d1"),
            is_current: true,
            name: text("Рабочий ноутбук"),
            number: 1,
        },
        SecurityDevice {
            added_at: text("2 сентября"),
            can_revoke: true,
            id: text("d2"),
            is_current: false,
            name: text("Телефон"),
            number: 2,
        },
    ]));
    ui.set_security_events(model(vec![
        SecurityEvent {
            created_at: text("Сегодня, 09:12"),
            detail: text("192.168.1.10"),
            event_type: text("login_ok"),
            id: text("e1"),
        },
        SecurityEvent {
            created_at: text("Вчера, 21:04"),
            detail: text("192.168.1.10"),
            event_type: text("totp_enabled"),
            id: text("e2"),
        },
    ]));
    ui.set_notifications_sound_options(model(vec![
        SettingsOption {
            label: text("Колокольчик"),
            value: text("chime"),
        },
        SettingsOption {
            label: text("Мягкий"),
            value: text("soft"),
        },
        SettingsOption {
            label: text("Без звука"),
            value: text("none"),
        },
    ]));
    ui.set_notifications_push_status(text("Подписка активна"));

    ui.set_settings_accounts(model(vec![
        SettingsAccount {
            avatar: Image::default(),
            fh_number: text("FH25822288"),
            has_avatar: false,
            id: text("a1"),
            pending_removal: false,
            username: text("Som_Somovich"),
        },
        SettingsAccount {
            avatar: Image::default(),
            fh_number: text("FH1002233"),
            has_avatar: false,
            id: text("a2"),
            pending_removal: false,
            username: text("work_account"),
        },
    ]));
    ui.set_settings_active_account_id(text("a1"));
    ui.set_settings_bots(model(vec![
        SettingsBot {
            handle: text("helper_bot"),
            id: text("bot1"),
            token_prefix: text("fh_bot_8f2c…"),
        },
        SettingsBot {
            handle: text("reminder_bot"),
            id: text("bot2"),
            token_prefix: text("fh_bot_11aa…"),
        },
    ]));
    ui.set_about_app_version(text("0.2.0"));
    ui.set_about_update_status(text("Установлена последняя версия"));

    ui.set_twofactor_setup_enabled(true);
    ui.set_twofactor_setup_enrollment_started(true);
    ui.set_twofactor_setup_secret_base32(text("JBSWY3DPEHPK3PXP"));
    ui.set_twofactor_setup_backup_codes(model(vec![
        text("4F7K-92QL"),
        text("8M1P-33XZ"),
        text("0C6R-77TV"),
        text("2N9D-51HB"),
    ]));
    ui.set_twofactor_setup_show_backup_codes(true);

    ui.set_connect_keys_state(ConnectStepState::Done);
    ui.set_connect_socket_state(ConnectStepState::Active);
    ui.set_connect_error(text(""));
    ui.set_login_fh_number(text("FH25822288"));
    ui.set_login_password(text("super-secret-password"));
    ui.set_register_password(text("super-secret-password"));
    ui.set_twofactor_code(text("123456"));
    ui.set_invite_token(text("fhinvite_7d3c"));

    ui.set_create_group_expiry_summary(text("Без срока"));
    ui.set_temporary_room_duration_valid(true);
}

fn conversations() -> Vec<ConversationRow> {
    vec![
        row(
            "dm-alice",
            "Алиса Соколова",
            "АС",
            "Созвон в 19:00, не опаздывай",
            "12:04",
            2,
            false,
            false,
            "alice",
        ),
        row(
            "grp-team",
            "Команда FriendsHub",
            "КФ",
            "Максим: выкатил новую сборку",
            "11:48",
            5,
            false,
            true,
            "",
        ),
        row(
            "ch-news",
            "Новости продукта",
            "НП",
            "Релиз 0.2.0 уже доступен",
            "10:22",
            0,
            true,
            false,
            "",
        ),
        row(
            "dm-bob",
            "Боб",
            "Б",
            "Скинешь макеты позже?",
            "Вчера",
            0,
            false,
            false,
            "bob",
        ),
        row(
            "dm-carol",
            "Кэрол",
            "К",
            "Спасибо!",
            "Вчера",
            0,
            false,
            false,
            "carol",
        ),
        row(
            "grp-design",
            "Дизайн-ревью",
            "ДР",
            "Кэрол: обновила иконки",
            "Пн",
            0,
            false,
            false,
            "",
        ),
        row(
            "dm-max",
            "Максим",
            "М",
            "Голосовое сообщение",
            "Вс",
            0,
            true,
            false,
            "max",
        ),
        row(
            "ch-rust",
            "Rust Digest",
            "РД",
            "Выпуск 42: async без боли",
            "Сб",
            0,
            false,
            false,
            "",
        ),
    ]
}

fn row(
    id: &str,
    title: &str,
    avatar_label: &str,
    preview: &str,
    timestamp: &str,
    unread_count: i32,
    muted: bool,
    has_pinned: bool,
    peer_account_id: &str,
) -> ConversationRow {
    ConversationRow {
        id: text(id),
        title: text(title),
        avatar_label: text(avatar_label),
        avatar_image: Image::default(),
        has_avatar: false,
        peer_account_id: text(peer_account_id),
        preview: text(preview),
        timestamp: text(timestamp),
        unread_count,
        muted,
        has_pinned,
        archived: false,
    }
}

fn filters() -> Vec<FilterTab> {
    vec![
        FilterTab {
            id: text("all"),
            label: text("Все"),
            unread_count: 0,
            visible: true,
        },
        FilterTab {
            id: text("unread"),
            label: text("Непрочитанные"),
            unread_count: 7,
            visible: true,
        },
        FilterTab {
            id: text("personal"),
            label: text("Личные"),
            unread_count: 0,
            visible: true,
        },
        FilterTab {
            id: text("groups"),
            label: text("Группы"),
            unread_count: 5,
            visible: true,
        },
        FilterTab {
            id: text("channels"),
            label: text("Каналы"),
            unread_count: 0,
            visible: true,
        },
    ]
}

fn messages() -> Vec<ChatMessage> {
    vec![
        message(
            "m1",
            false,
            false,
            "Алиса",
            "",
            "Привет! Сможешь сегодня в 19:00 на созвон?",
            "11:42",
            true,
            "Сегодня",
            "Доставлено",
            vec![],
            vec![],
        ),
        message(
            "m2",
            true,
            false,
            "Som_Somovich",
            "",
            "Привет! Да, давай. Скинь ссылку, если не сложно.",
            "11:45",
            false,
            "",
            "Прочитано",
            vec![],
            vec![ChatReaction {
                count_label: text("1"),
                emoji: text("👍"),
                mine: false,
            }],
        ),
        message(
            "m3",
            false,
            false,
            "Алиса",
            "",
            "Созвон в 19:00, не опаздывай",
            "12:04",
            false,
            "",
            "Доставлено",
            vec![],
            vec![],
        ),
        message(
            "m4",
            true,
            false,
            "Som_Somovich",
            "",
            "Договорились. До встречи!",
            "12:05",
            false,
            "",
            "Отправлено",
            vec![],
            vec![],
        ),
        message(
            "m5",
            false,
            false,
            "Алиса",
            "",
            "Кстати, вот файл с макетами — посмотри, пожалуйста.",
            "12:06",
            false,
            "",
            "",
            vec![ChatAttachmentView {
                has_preview: false,
                id: text("a1"),
                label: text("friendshub-mockups.pdf"),
                preview_image: Image::default(),
                size_label: text("2,4 МБ"),
                state_label: text(""),
                unavailable: false,
            }],
            vec![],
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn message(
    id: &str,
    own: bool,
    edited: bool,
    author: &str,
    author_tag: &str,
    body: &str,
    time_label: &str,
    show_day_label: bool,
    day_label: &str,
    delivery_label: &str,
    attachments: Vec<ChatAttachmentView>,
    reactions: Vec<ChatReaction>,
) -> ChatMessage {
    ChatMessage {
        attachments: model(attachments),
        author: text(author),
        author_tag: text(author_tag),
        body: text(body),
        day_label: text(day_label),
        delivery_label: text(delivery_label),
        edited,
        id: text(id),
        own,
        reactions: model(reactions),
        show_day_label,
        time_label: text(time_label),
    }
}

fn poll() -> PollViewModel {
    PollViewModel {
        add_option_label: text("Добавить вариант"),
        allows_multiple: false,
        busy: false,
        can_propose: true,
        can_submit_vote: true,
        can_vote: true,
        cannot_decrypt_label: text(""),
        close_label: text("Закрыть опрос"),
        closed: false,
        closes_at_label: text("до 20:00"),
        correct_label: text("Верно"),
        delete_description: text("Удалить опрос?"),
        delete_label: text("Удалить"),
        description: text("Выберите удобное время для созвона"),
        error_text: text(""),
        has_voted: true,
        id: text("poll-1"),
        option_unavailable_label: text("Недоступно"),
        options: model(vec![
            PollOptionView {
                can_remove: false,
                correct: false,
                count_label: text("5"),
                id: text("o1"),
                percent_label: text("50%"),
                percentage: 50,
                selected: true,
                text: text("19:00"),
                user_proposed: false,
            },
            PollOptionView {
                can_remove: false,
                correct: false,
                count_label: text("3"),
                id: text("o2"),
                percent_label: text("30%"),
                percentage: 30,
                selected: false,
                text: text("20:00"),
                user_proposed: false,
            },
            PollOptionView {
                can_remove: false,
                correct: false,
                count_label: text("2"),
                id: text("o3"),
                percent_label: text("20%"),
                percentage: 20,
                selected: false,
                text: text("21:00"),
                user_proposed: true,
            },
        ]),
        owner: false,
        proposal_valid: true,
        propose_label: text("Предложить вариант"),
        show_results: true,
        stop_label: text("Завершить"),
        title: text("Когда созваниваемся?"),
        total_voters_label: text("10 голосов"),
        type_label: text("Опрос"),
        vote_label: text("Голосовать"),
        your_vote_label: text("Ваш голос: 19:00"),
    }
}

fn channel_posts() -> Vec<ChannelPost> {
    vec![
        ChannelPost {
            attachments: model(vec![]),
            author: text("Команда FriendsHub"),
            body: text(
                "Релиз 0.2.0 уже доступен: нативные клиенты получили обновлённый интерфейс, ускоренную синхронизацию и новые настройки приватности.",
            ),
            discussion_label: text("Обсудить · 12 комментариев"),
            edited: false,
            id: text("p1"),
            pinned: true,
            reactions: model(vec![
                ChatReaction {
                    count_label: text("24"),
                    emoji: text("🎉"),
                    mine: true,
                },
                ChatReaction {
                    count_label: text("8"),
                    emoji: text("❤️"),
                    mine: false,
                },
            ]),
            time_label: text("12:00"),
        },
        ChannelPost {
            attachments: model(vec![]),
            author: text("Команда FriendsHub"),
            body: text(
                "На этой неделе мы переработали экран настроек: стало проще найти нужный раздел, а изменения применяются без перезапуска.",
            ),
            discussion_label: text("Обсудить · 4 комментария"),
            edited: false,
            id: text("p2"),
            pinned: false,
            reactions: model(vec![]),
            time_label: text("Вчера"),
        },
    ]
}

fn calls() -> Vec<CallHistoryRow> {
    vec![
        CallHistoryRow {
            completed: true,
            direction_status: text("Входящий · 12:34"),
            duration: text("14 мин"),
            id: text("c1"),
            peer_account_id: text("alice"),
            peer_initial: text("АС"),
            peer_name: text("Алиса Соколова"),
            started_at: text("Сегодня, 12:34"),
        },
        CallHistoryRow {
            completed: false,
            direction_status: text("Исходящий · нет ответа"),
            duration: text(""),
            id: text("c2"),
            peer_account_id: text("bob"),
            peer_initial: text("Б"),
            peer_name: text("Боб"),
            started_at: text("Вчера, 18:02"),
        },
        CallHistoryRow {
            completed: true,
            direction_status: text("Исходящий · 10:15"),
            duration: text("3 мин"),
            id: text("c3"),
            peer_account_id: text("carol"),
            peer_initial: text("К"),
            peer_name: text("Кэрол"),
            started_at: text("Вчера, 10:15"),
        },
    ]
}

fn contacts() -> Vec<SettingsContact> {
    vec![
        SettingsContact {
            id: text("alice"),
            name: text("Алиса Соколова"),
        },
        SettingsContact {
            id: text("bob"),
            name: text("Боб"),
        },
        SettingsContact {
            id: text("carol"),
            name: text("Кэрол"),
        },
    ]
}
